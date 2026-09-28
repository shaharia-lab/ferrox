//! Per-request accounting shared by the inbound handlers.
//!
//! Every inbound surface (`/v1/chat/completions`, `/anthropic/v1/messages`,
//! `/v1/responses`)
//! dispatches through the same OpenAI-format pipeline and must account for the
//! request identically: token and latency metrics, the `usage_log` row, the
//! `token_usage` webhook and the budget reconciliation. That logic lives here,
//! once; each handler keeps only its own wire encoding.
//!
//! - [`RequestFinalizer::finish`] closes a non-streaming request.
//! - [`RequestFinalizer::wrap_stream`] meters a [`ProviderStream`](crate::providers::ProviderStream) and closes the
//!   request exactly once — when the upstream stream ends (before the handler
//!   emits its terminal frame), or when the stream is dropped part-way because
//!   the client disconnected or the body errored.
//! - [`record_error_metrics`] accounts for a request that failed to dispatch.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{ready, Context, Poll};
use std::time::Instant;

use futures::future::BoxFuture;
use futures::stream::BoxStream;
use futures::{Stream, StreamExt as _};
use uuid::Uuid;

use crate::budget_enforcer::BudgetEnforcer;
use crate::error::ProxyError;
use crate::event_dispatcher::{EventDispatcher, TokenUsageEvent};
use crate::state::AppState;
use crate::telemetry::metrics::{
    self, ACTIVE_STREAMS, ERRORS_TOTAL, REQUESTS_TOTAL, REQUEST_DURATION_SECONDS,
};
use crate::types::{ChatCompletionChunk, RequestContext, Usage};
use crate::usage_writer::{UsageEvent, UsageWriter};
use ferrox_providers::responses_types::NativeResponsesEvent;

/// Which inbound surface a request arrived on. Only selects the log message,
/// so existing log queries keep matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Surface {
    OpenAi,
    Anthropic,
    Responses,
}

/// Plain token counts read once from a [`Usage`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TokenCounts {
    prompt: u32,
    completion: u32,
    cache_read: u32,
    cache_write: u32,
}

impl TokenCounts {
    fn from_usage(usage: &Usage) -> Self {
        let (cache_read, cache_write) = crate::types::cache_tokens(usage);
        Self {
            prompt: usage.prompt_tokens,
            completion: usage.completion_tokens,
            cache_read,
            cache_write,
        }
    }
}

/// Everything needed to account for one successfully dispatched request.
pub(crate) struct RequestFinalizer {
    usage_writer: UsageWriter,
    budget_enforcer: Arc<dyn BudgetEnforcer>,
    event_dispatcher: EventDispatcher,
    request_id: String,
    key_name: String,
    client_id: Option<Uuid>,
    budget_period: Option<String>,
    budget_reserved_tokens: u32,
    model_alias: String,
    provider: String,
    model_id: String,
    start: Instant,
    surface: Surface,
}

/// A budget reconciliation owed once the request's usage is known.
struct Reconcile {
    enforcer: Arc<dyn BudgetEnforcer>,
    client_id: String,
    period: String,
    reserved: u32,
    actual: u32,
}

impl Reconcile {
    async fn run(self) {
        self.enforcer
            .reconcile_tokens(&self.client_id, &self.period, self.reserved, self.actual)
            .await;
    }
}

impl RequestFinalizer {
    pub(crate) fn new(
        state: &AppState,
        ctx: &RequestContext,
        model_alias: String,
        provider: String,
        model_id: String,
        start: Instant,
        surface: Surface,
    ) -> Self {
        Self::from_parts(
            state.usage_writer.clone(),
            state.budget_enforcer.clone(),
            state.event_dispatcher.clone(),
            ctx,
            model_alias,
            provider,
            model_id,
            start,
            surface,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn from_parts(
        usage_writer: UsageWriter,
        budget_enforcer: Arc<dyn BudgetEnforcer>,
        event_dispatcher: EventDispatcher,
        ctx: &RequestContext,
        model_alias: String,
        provider: String,
        model_id: String,
        start: Instant,
        surface: Surface,
    ) -> Self {
        Self {
            usage_writer,
            budget_enforcer,
            event_dispatcher,
            request_id: ctx.request_id.clone(),
            key_name: ctx.key_name.clone(),
            client_id: ctx.client_id,
            budget_period: ctx.budget_period.clone(),
            budget_reserved_tokens: ctx.budget_reserved_tokens,
            model_alias,
            provider,
            model_id,
            start,
            surface,
        }
    }

    /// Close a non-streaming request: token metrics, request metrics, usage row,
    /// webhook and budget reconciliation. Usage is recorded whenever the
    /// provider returned a `usage` object.
    pub(crate) async fn finish(self, usage: Option<&Usage>) {
        let counts = usage.map(TokenCounts::from_usage);
        if let Some(c) = counts {
            self.record_token_metrics(c);
        }
        if let Some(reconcile) = self.settle(counts, false) {
            reconcile.run().await;
        }
    }

    /// Meter a provider stream — chat chunks, or a native Responses event
    /// stream — and close the request exactly once.
    ///
    /// Increments `ferrox_active_streams` now; the wrapper decrements it when
    /// it finalizes.
    pub(crate) fn wrap_stream<T: Metered>(
        self,
        inner: BoxStream<'static, Result<T, ProxyError>>,
    ) -> FinalizedStream<T> {
        ACTIVE_STREAMS
            .with_label_values(&[self.provider.as_str(), self.model_alias.as_str()])
            .inc();
        FinalizedStream {
            inner,
            finalizer: Some(self),
            last_usage: None,
            reconcile: None,
        }
    }

    fn record_token_metrics(&self, c: TokenCounts) {
        metrics::record_tokens(
            &self.provider,
            &self.model_alias,
            &self.key_name,
            c.prompt,
            c.completion,
            c.cache_read,
            c.cache_write,
        );
    }

    /// Record everything that does not need to be awaited and return the
    /// budget reconciliation still owed, if any.
    fn settle(self, counts: Option<TokenCounts>, streaming: bool) -> Option<Reconcile> {
        let latency = self.start.elapsed().as_secs_f64();
        let latency_ms = (latency * 1000.0) as u64;
        let provider = self.provider.as_str();
        let alias = self.model_alias.as_str();

        REQUESTS_TOTAL
            .with_label_values(&[
                provider,
                alias,
                self.model_id.as_str(),
                "200",
                &self.key_name,
            ])
            .inc();
        REQUEST_DURATION_SECONDS
            .with_label_values(&[provider, alias, "200"])
            .observe(latency);
        if streaming {
            ACTIVE_STREAMS.with_label_values(&[provider, alias]).dec();
        }

        let (prompt, completion, cache_read, cache_write) = counts
            .map(|c| (c.prompt, c.completion, c.cache_read, c.cache_write))
            .unwrap_or((0, 0, 0, 0));
        // `Option` fields are omitted entirely when `None`, so quiet
        // (non-caching) paths stay quiet.
        let cache_read_log = (cache_read > 0).then_some(cache_read);
        let cache_write_log = (cache_write > 0).then_some(cache_write);
        // One field list for every surface; only the message differs, so
        // existing log queries keep matching.
        macro_rules! completed {
            ($msg:literal) => {
                tracing::info!(
                    request_id = %self.request_id,
                    key_name = %self.key_name,
                    model_alias = %alias,
                    provider = %provider,
                    model_id = %self.model_id,
                    streaming,
                    status = 200,
                    latency_ms,
                    prompt_tokens = prompt,
                    completion_tokens = completion,
                    cache_read_tokens = cache_read_log,
                    cache_write_tokens = cache_write_log,
                    $msg
                )
            };
        }
        match self.surface {
            Surface::OpenAi => completed!("request_completed"),
            Surface::Anthropic => completed!("anthropic_request_completed"),
            Surface::Responses => completed!("responses_request_completed"),
        }

        let c = counts?;
        self.event_dispatcher.dispatch(TokenUsageEvent {
            event: "token_usage",
            request_id: self.request_id.clone(),
            client_id: self.client_id,
            key_name: self.key_name,
            model: self.model_alias.clone(),
            provider: self.provider.clone(),
            prompt_tokens: c.prompt,
            completion_tokens: c.completion,
            total_tokens: c.prompt + c.completion,
            cache_read_tokens: (c.cache_read > 0).then_some(c.cache_read),
            cache_write_tokens: (c.cache_write > 0).then_some(c.cache_write),
            latency_ms: Some(latency_ms),
            timestamp: chrono::Utc::now(),
        });
        self.usage_writer.record(UsageEvent {
            client_id: self.client_id,
            request_id: self.request_id,
            model: self.model_alias,
            provider: self.provider,
            prompt_tokens: c.prompt,
            completion_tokens: c.completion,
            cache_read_tokens: c.cache_read,
            cache_write_tokens: c.cache_write,
            latency_ms: Some(latency_ms),
        });

        match (self.client_id, self.budget_period) {
            (Some(cid), Some(period)) => Some(Reconcile {
                enforcer: self.budget_enforcer,
                client_id: cid.to_string(),
                period,
                reserved: self.budget_reserved_tokens,
                actual: c.prompt + c.completion,
            }),
            _ => None,
        }
    }
}

/// A stream item that may carry the request's token usage: a chat chunk, or
/// the terminal event of a native Responses stream.
pub(crate) trait Metered {
    fn usage(&self) -> Option<&Usage>;
}

impl Metered for ChatCompletionChunk {
    fn usage(&self) -> Option<&Usage> {
        self.usage.as_ref()
    }
}

impl Metered for NativeResponsesEvent {
    fn usage(&self) -> Option<&Usage> {
        self.usage.as_ref()
    }
}

/// A [`ProviderStream`](crate::providers::ProviderStream) (or any other [`Metered`] stream) that records token
/// metrics from each item carrying `usage`, and finalizes the request exactly once: when the upstream ends
/// (the budget reconciliation is awaited before this stream reports its end,
/// so a handler's chained terminal frame always follows the accounting), or
/// on drop if it never got that far.
///
/// The once-guard is the `Option` around the finalizer; no locks, and nothing
/// is allocated per chunk.
pub(crate) struct FinalizedStream<T = ChatCompletionChunk> {
    inner: BoxStream<'static, Result<T, ProxyError>>,
    finalizer: Option<RequestFinalizer>,
    last_usage: Option<TokenCounts>,
    reconcile: Option<BoxFuture<'static, ()>>,
}

impl<T> FinalizedStream<T> {
    /// Settle the request if it has not been already. A stream that never saw
    /// non-zero prompt or completion tokens records no usage.
    fn settle(&mut self) -> Option<BoxFuture<'static, ()>> {
        let finalizer = self.finalizer.take()?;
        let counts = self.last_usage.filter(|c| c.prompt > 0 || c.completion > 0);
        finalizer
            .settle(counts, true)
            .map(|r| Box::pin(r.run()) as BoxFuture<'static, ()>)
    }
}

impl<T: Metered> Stream for FinalizedStream<T> {
    type Item = Result<T, ProxyError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        loop {
            if let Some(fut) = this.reconcile.as_mut() {
                ready!(fut.as_mut().poll(cx));
                this.reconcile = None;
                return Poll::Ready(None);
            }
            let Some(finalizer) = this.finalizer.as_ref() else {
                return Poll::Ready(None);
            };
            match ready!(this.inner.poll_next_unpin(cx)) {
                Some(Ok(chunk)) => {
                    if let Some(usage) = chunk.usage() {
                        let c = TokenCounts::from_usage(usage);
                        finalizer.record_token_metrics(c);
                        this.last_usage = Some(c);
                    }
                    return Poll::Ready(Some(Ok(chunk)));
                }
                Some(Err(e)) => return Poll::Ready(Some(Err(e))),
                None => match this.settle() {
                    Some(fut) => this.reconcile = Some(fut),
                    None => return Poll::Ready(None),
                },
            }
        }
    }
}

impl<T> Drop for FinalizedStream<T> {
    fn drop(&mut self) {
        // Client disconnected, the body errored, or the reconciliation was
        // still in flight: finish the accounting, handing any owed
        // reconciliation to the runtime since `drop` cannot await it.
        let pending = self.reconcile.take().or_else(|| self.settle());
        if let (Some(fut), Ok(handle)) = (pending, tokio::runtime::Handle::try_current()) {
            handle.spawn(fut);
        }
    }
}

// ── Failed dispatch ──────────────────────────────────────────────────────────

/// Record the request, duration and error metrics for a request that failed
/// before any target served it.
pub(crate) fn record_error_metrics(
    model_alias: &str,
    provider: &str,
    e: &ProxyError,
    start: Instant,
) {
    let status_code = http_status_for_error(e).to_string();
    record_error_counters(model_alias, provider, e, &status_code);
    REQUEST_DURATION_SECONDS
        .with_label_values(&[provider, model_alias, &status_code])
        .observe(start.elapsed().as_secs_f64());
}

/// The request and error counters of [`record_error_metrics`], without the
/// duration observation. Only the Anthropic streaming path uses this directly;
/// it has never observed a duration on dispatch failure.
pub(crate) fn record_error_count(model_alias: &str, provider: &str, e: &ProxyError) {
    record_error_counters(
        model_alias,
        provider,
        e,
        &http_status_for_error(e).to_string(),
    );
}

fn record_error_counters(model_alias: &str, provider: &str, e: &ProxyError, status_code: &str) {
    REQUESTS_TOTAL
        .with_label_values(&[provider, model_alias, "", status_code, ""])
        .inc();
    ERRORS_TOTAL
        .with_label_values(&[provider, error_type_label(e)])
        .inc();
}

pub(crate) fn http_status_for_error(e: &ProxyError) -> u16 {
    match e {
        ProxyError::Unauthorized(_) => 401,
        ProxyError::Forbidden(_) => 403,
        ProxyError::InvalidRequest { .. } => 400,
        ProxyError::ModelNotFound(_) => 404,
        ProxyError::RateLimited(_) | ProxyError::BudgetExceeded(_) => 429,
        ProxyError::CircuitOpen(_) | ProxyError::ProviderError { .. } => 502,
        ProxyError::UpstreamTimeout(_) => 504,
        _ => 500,
    }
}

pub(crate) fn error_type_label(e: &ProxyError) -> &'static str {
    match e {
        ProxyError::Unauthorized(_) => "unauthorized",
        ProxyError::Forbidden(_) => "forbidden",
        ProxyError::InvalidRequest { .. } => "invalid_request",
        ProxyError::ModelNotFound(_) => "model_not_found",
        ProxyError::RateLimited(_) => "rate_limited",
        ProxyError::BudgetExceeded(_) => "budget_exceeded",
        ProxyError::CircuitOpen(_) => "circuit_open",
        ProxyError::ProviderError { .. } => "provider_error",
        ProxyError::UpstreamTimeout(_) => "upstream_timeout",
        ProxyError::StreamError(_) => "stream_error",
        ProxyError::HttpClientError(_) => "http_client_error",
        ProxyError::AwsError(_) => "aws_error",
        _ => "internal",
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use tokio::sync::mpsc;

    use super::*;

    #[derive(Default)]
    struct FakeBudget {
        calls: Mutex<Vec<(String, String, u32, u32)>>,
    }

    #[async_trait]
    impl BudgetEnforcer for FakeBudget {
        async fn reserve_tokens(&self, _: &str, _: &str, _: i64, _: u32) -> Result<(), ()> {
            Ok(())
        }
        async fn reconcile_tokens(
            &self,
            client_id: &str,
            period: &str,
            reserved: u32,
            actual: u32,
        ) {
            self.calls.lock().unwrap().push((
                client_id.to_string(),
                period.to_string(),
                reserved,
                actual,
            ));
        }
    }

    struct Harness {
        usage_rx: mpsc::Receiver<UsageEvent>,
        event_rx: mpsc::Receiver<TokenUsageEvent>,
        budget: Arc<FakeBudget>,
        client_id: Uuid,
        finalizer: Option<RequestFinalizer>,
    }

    impl Harness {
        /// Each test uses its own `alias` so the global Prometheus series it
        /// asserts on are not shared with any other test.
        fn new(alias: &str) -> Self {
            let (usage_writer, usage_rx) = UsageWriter::channel(8);
            let (event_dispatcher, event_rx) = EventDispatcher::channel(8);
            let budget = Arc::new(FakeBudget::default());
            let client_id = Uuid::new_v4();
            let ctx = RequestContext {
                request_id: "req-1".into(),
                key_name: "key-1".into(),
                allowed_models: vec!["*".into()],
                client_id: Some(client_id),
                token_budget: Some(1000),
                budget_period: Some("daily".into()),
                budget_reserved_tokens: 64,
            };
            let finalizer = RequestFinalizer::from_parts(
                usage_writer,
                budget.clone(),
                event_dispatcher,
                &ctx,
                alias.into(),
                "prov".into(),
                "model-x".into(),
                Instant::now(),
                Surface::OpenAi,
            );
            Self {
                usage_rx,
                event_rx,
                budget,
                client_id,
                finalizer: Some(finalizer),
            }
        }

        fn take(&mut self) -> RequestFinalizer {
            self.finalizer.take().unwrap()
        }

        fn reconciles(&self) -> Vec<(String, String, u32, u32)> {
            self.budget.calls.lock().unwrap().clone()
        }
    }

    fn usage(prompt: u32, completion: u32) -> Usage {
        let mut extra = std::collections::HashMap::new();
        extra.insert("cache_read_input_tokens".to_string(), serde_json::json!(7));
        Usage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: prompt + completion,
            extra,
        }
    }

    fn chunk(usage: Option<Usage>) -> Result<ChatCompletionChunk, ProxyError> {
        Ok(ChatCompletionChunk {
            id: "c".into(),
            object: "chat.completion.chunk".into(),
            created: 0,
            model: "m".into(),
            choices: vec![],
            usage,
            extra: Default::default(),
        })
    }

    fn requests_total(alias: &str) -> f64 {
        REQUESTS_TOTAL
            .with_label_values(&["prov", alias, "model-x", "200", "key-1"])
            .get()
    }

    fn active_streams(alias: &str) -> f64 {
        ACTIVE_STREAMS.with_label_values(&["prov", alias]).get()
    }

    #[tokio::test]
    async fn finish_with_usage_records_everything_once() {
        let mut h = Harness::new("fin-usage");
        h.take().finish(Some(&usage(10, 5))).await;

        let row = h.usage_rx.try_recv().expect("usage row");
        assert_eq!(row.client_id, Some(h.client_id));
        assert_eq!(row.request_id, "req-1");
        assert_eq!(row.model, "fin-usage");
        assert_eq!(row.provider, "prov");
        assert_eq!((row.prompt_tokens, row.completion_tokens), (10, 5));
        assert_eq!((row.cache_read_tokens, row.cache_write_tokens), (7, 0));
        assert!(row.latency_ms.is_some());
        assert!(h.usage_rx.try_recv().is_err());

        let ev = h.event_rx.try_recv().expect("webhook event");
        assert_eq!(ev.event, "token_usage");
        assert_eq!(ev.key_name, "key-1");
        assert_eq!(ev.total_tokens, 15);
        assert_eq!(ev.cache_read_tokens, Some(7));
        assert_eq!(ev.cache_write_tokens, None);
        assert!(h.event_rx.try_recv().is_err());

        assert_eq!(
            h.reconciles(),
            vec![(h.client_id.to_string(), "daily".to_string(), 64, 15)]
        );
        assert_eq!(requests_total("fin-usage"), 1.0);
        assert_eq!(
            metrics::TOKENS_TOTAL
                .with_label_values(&["prov", "fin-usage", "key-1", "cache_read"])
                .get(),
            7.0
        );
    }

    #[tokio::test]
    async fn finish_without_usage_records_only_request_metrics() {
        let mut h = Harness::new("fin-none");
        h.take().finish(None).await;

        assert!(h.usage_rx.try_recv().is_err());
        assert!(h.event_rx.try_recv().is_err());
        assert!(h.reconciles().is_empty());
        assert_eq!(requests_total("fin-none"), 1.0);
    }

    #[tokio::test]
    async fn stream_accounts_before_terminal_frame_and_exactly_once() {
        let mut h = Harness::new("stream-full");
        let upstream = futures::stream::iter(vec![chunk(None), chunk(Some(usage(3, 4)))]).boxed();
        let wrapped = h.take().wrap_stream(upstream);
        assert_eq!(active_streams("stream-full"), 1.0);

        // Mirrors the handlers: the terminal frame is chained after the
        // wrapper, so it may only be produced once the accounting is done.
        let budget = h.budget.clone();
        let terminal = futures::stream::once(async move {
            assert_eq!(
                budget.calls.lock().unwrap().len(),
                1,
                "reconciled before terminal"
            );
            Err(ProxyError::StreamError("terminal".into()))
        });
        let mut out = Box::pin(wrapped.chain(terminal));
        let mut items = 0;
        while let Some(item) = out.next().await {
            items += 1;
            if items == 3 {
                assert!(item.is_err(), "terminal frame last");
            }
        }
        assert_eq!(items, 3);
        let row = h
            .usage_rx
            .try_recv()
            .expect("usage row before terminal frame");
        assert_eq!((row.prompt_tokens, row.completion_tokens), (3, 4));

        drop(out);
        tokio::task::yield_now().await;
        assert!(
            h.usage_rx.try_recv().is_err(),
            "no second usage row on drop"
        );
        assert!(h.event_rx.try_recv().is_ok());
        assert!(h.event_rx.try_recv().is_err(), "no second webhook on drop");
        assert_eq!(h.reconciles().len(), 1);
        assert_eq!(requests_total("stream-full"), 1.0);
        assert_eq!(active_streams("stream-full"), 0.0);
    }

    #[tokio::test]
    async fn stream_dropped_part_way_is_finalized_once() {
        let mut h = Harness::new("stream-drop");
        let upstream = futures::stream::iter(vec![chunk(Some(usage(2, 1)))])
            .chain(futures::stream::pending())
            .boxed();
        let mut wrapped = h.take().wrap_stream(upstream);
        assert!(wrapped.next().await.unwrap().is_ok());
        assert!(h.usage_rx.try_recv().is_err(), "not finalized while open");

        drop(wrapped);
        // The reconciliation is spawned from `drop`; let it run.
        tokio::task::yield_now().await;

        let row = h.usage_rx.try_recv().expect("usage row on drop");
        assert_eq!((row.prompt_tokens, row.completion_tokens), (2, 1));
        assert!(h.usage_rx.try_recv().is_err());
        assert_eq!(h.reconciles().len(), 1);
        assert_eq!(requests_total("stream-drop"), 1.0);
        assert_eq!(active_streams("stream-drop"), 0.0);
    }

    #[tokio::test]
    async fn stream_error_part_way_is_finalized_on_drop() {
        let mut h = Harness::new("stream-err");
        let upstream = futures::stream::iter(vec![
            chunk(Some(usage(1, 1))),
            Err(ProxyError::StreamError("boom".into())),
        ])
        .boxed();
        let mut wrapped = h.take().wrap_stream(upstream);
        assert!(wrapped.next().await.unwrap().is_ok());
        assert!(wrapped.next().await.unwrap().is_err());
        drop(wrapped);
        tokio::task::yield_now().await;

        assert!(h.usage_rx.try_recv().is_ok());
        assert!(h.usage_rx.try_recv().is_err());
        assert_eq!(requests_total("stream-err"), 1.0);
        assert_eq!(active_streams("stream-err"), 0.0);
    }

    #[tokio::test]
    async fn stream_with_zero_usage_records_no_usage() {
        let mut h = Harness::new("stream-zero");
        let upstream = futures::stream::iter(vec![chunk(Some(usage(0, 0)))]).boxed();
        let collected: Vec<_> = h.take().wrap_stream(upstream).collect().await;
        assert_eq!(collected.len(), 1);

        assert!(h.usage_rx.try_recv().is_err());
        assert!(h.event_rx.try_recv().is_err());
        assert!(h.reconciles().is_empty());
        assert_eq!(requests_total("stream-zero"), 1.0);
        assert_eq!(active_streams("stream-zero"), 0.0);
    }

    #[test]
    fn record_error_metrics_uses_error_labels() {
        let e = ProxyError::UpstreamTimeout("slow".into());
        record_error_metrics("err-alias", "", &e, Instant::now());
        assert_eq!(
            REQUESTS_TOTAL
                .with_label_values(&["", "err-alias", "", "504", ""])
                .get(),
            1.0
        );
        assert_eq!(
            REQUEST_DURATION_SECONDS
                .with_label_values(&["", "err-alias", "504"])
                .get_sample_count(),
            1
        );

        record_error_count("err-count-alias", "", &e);
        assert_eq!(
            REQUESTS_TOTAL
                .with_label_values(&["", "err-count-alias", "", "504", ""])
                .get(),
            1.0
        );
        assert_eq!(
            REQUEST_DURATION_SECONDS
                .with_label_values(&["", "err-count-alias", "504"])
                .get_sample_count(),
            0
        );
    }

    #[test]
    fn error_status_and_type_mapping() {
        let cases = [
            (ProxyError::Unauthorized("x".into()), 401, "unauthorized"),
            (ProxyError::Forbidden("x".into()), 403, "forbidden"),
            (
                ProxyError::InvalidRequest {
                    message: "x".into(),
                    param: None,
                },
                400,
                "invalid_request",
            ),
            (
                ProxyError::ModelNotFound("x".into()),
                404,
                "model_not_found",
            ),
            (ProxyError::RateLimited("x".into()), 429, "rate_limited"),
            (
                ProxyError::BudgetExceeded("x".into()),
                429,
                "budget_exceeded",
            ),
            (ProxyError::CircuitOpen("x".into()), 502, "circuit_open"),
            (
                ProxyError::UpstreamTimeout("x".into()),
                504,
                "upstream_timeout",
            ),
            (ProxyError::StreamError("x".into()), 500, "stream_error"),
        ];
        for (e, status, label) in cases {
            assert_eq!(http_status_for_error(&e), status, "{e:?}");
            assert_eq!(error_type_label(&e), label, "{e:?}");
        }
    }
}
