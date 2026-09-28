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

use crate::budget_enforcer::{BudgetEnforcer, BudgetReservation, ClaimedReservation};
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
    event_dispatcher: EventDispatcher,
    request_id: String,
    key_name: String,
    client_id: Option<Uuid>,
    /// The pre-request budget reservation, claimed when the request settles.
    budget_reservation: Option<BudgetReservation>,
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
    fn new(claimed: ClaimedReservation, actual: u32) -> Self {
        Self {
            enforcer: claimed.enforcer,
            client_id: claimed.client_id,
            period: claimed.period,
            reserved: claimed.reserved,
            actual,
        }
    }

    async fn run(self) {
        self.enforcer
            .reconcile_tokens(&self.client_id, &self.period, self.reserved, self.actual)
            .await;
    }
}

impl RequestFinalizer {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        state: &AppState,
        ctx: &RequestContext,
        budget_reservation: Option<BudgetReservation>,
        model_alias: String,
        provider: String,
        model_id: String,
        start: Instant,
        surface: Surface,
    ) -> Self {
        Self::from_parts(
            state.usage_writer.clone(),
            state.event_dispatcher.clone(),
            ctx,
            budget_reservation,
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
        event_dispatcher: EventDispatcher,
        ctx: &RequestContext,
        budget_reservation: Option<BudgetReservation>,
        model_alias: String,
        provider: String,
        model_id: String,
        start: Instant,
        surface: Surface,
    ) -> Self {
        Self {
            usage_writer,
            event_dispatcher,
            request_id: ctx.request_id.clone(),
            key_name: ctx.key_name.clone(),
            client_id: ctx.client_id,
            budget_reservation,
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
            held: None,
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

        // Claim the reservation whatever the outcome, so it is not also
        // refunded on drop. Without usage the reserved estimate stays charged,
        // as it always has: the upstream served the request and may have
        // consumed tokens that were never reported.
        let claimed = self
            .budget_reservation
            .as_ref()
            .and_then(BudgetReservation::claim);
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

        claimed.map(|claimed| Reconcile::new(claimed, c.prompt + c.completion))
    }
}

/// A stream item that may carry the request's token usage: a chat chunk, or
/// the terminal event of a native Responses stream.
pub(crate) trait Metered {
    fn usage(&self) -> Option<&Usage>;

    /// Whether this item is the client-visible end of the answer, to be held
    /// back until the request is settled so the client never sees the answer
    /// complete before its budget is reconciled. Chat chunks are followed by
    /// the handler's own terminal frame, so they are never held.
    fn is_terminal(&self) -> bool {
        false
    }
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

    /// A native stream's terminal event carries the usage and is the last
    /// frame the client gets.
    fn is_terminal(&self) -> bool {
        self.usage.is_some()
    }
}

/// A [`ProviderStream`](crate::providers::ProviderStream) (or any other [`Metered`] stream) that records token
/// metrics from each item carrying `usage`, and finalizes the request exactly once: when the upstream ends
/// (the budget reconciliation is awaited before this stream reports its end,
/// so a handler's chained terminal frame — or a held [`Metered::is_terminal`]
/// item — always follows the accounting), or on drop if it never got that far.
///
/// The once-guard is the `Option` around the finalizer; no locks, and nothing
/// is allocated per chunk.
pub(crate) struct FinalizedStream<T = ChatCompletionChunk> {
    inner: BoxStream<'static, Result<T, ProxyError>>,
    finalizer: Option<RequestFinalizer>,
    last_usage: Option<TokenCounts>,
    reconcile: Option<BoxFuture<'static, ()>>,
    /// A terminal item waiting for the request to settle.
    held: Option<T>,
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

impl<T: Metered + Unpin> Stream for FinalizedStream<T> {
    type Item = Result<T, ProxyError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        loop {
            if let Some(fut) = this.reconcile.as_mut() {
                ready!(fut.as_mut().poll(cx));
                this.reconcile = None;
                return Poll::Ready(this.held.take().map(Ok));
            }
            let Some(finalizer) = this.finalizer.as_ref() else {
                return Poll::Ready(this.held.take().map(Ok));
            };
            match ready!(this.inner.poll_next_unpin(cx)) {
                Some(Ok(chunk)) => {
                    if let Some(usage) = chunk.usage() {
                        let c = TokenCounts::from_usage(usage);
                        finalizer.record_token_metrics(c);
                        this.last_usage = Some(c);
                    }
                    if !chunk.is_terminal() {
                        return Poll::Ready(Some(Ok(chunk)));
                    }
                    // The answer is complete: settle now, release it after,
                    // and read nothing more from the upstream — a trailing
                    // frame must not overtake it.
                    this.held = Some(chunk);
                    match this.settle() {
                        Some(fut) => this.reconcile = Some(fut),
                        None => return Poll::Ready(this.held.take().map(Ok)),
                    }
                }
                Some(Err(e)) => return Poll::Ready(Some(Err(e))),
                None => match this.settle() {
                    Some(fut) => this.reconcile = Some(fut),
                    None => return Poll::Ready(this.held.take().map(Ok)),
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
    use tokio::sync::mpsc;

    use super::*;
    use crate::budget_enforcer::RecordingBudget;

    struct Harness {
        usage_rx: mpsc::Receiver<UsageEvent>,
        event_rx: mpsc::Receiver<TokenUsageEvent>,
        budget: Arc<RecordingBudget>,
        client_id: Uuid,
        finalizer: Option<RequestFinalizer>,
    }

    impl Harness {
        /// Each test uses its own `alias` so the global Prometheus series it
        /// asserts on are not shared with any other test.
        fn new(alias: &str) -> Self {
            let (usage_writer, usage_rx) = UsageWriter::channel(8);
            let (event_dispatcher, event_rx) = EventDispatcher::channel(8);
            let budget = Arc::new(RecordingBudget::default());
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
            let reservation =
                BudgetReservation::new(budget.clone(), client_id.to_string(), "daily".into(), 64);
            let finalizer = RequestFinalizer::from_parts(
                usage_writer,
                event_dispatcher,
                &ctx,
                Some(reservation),
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
            self.budget.reconciles()
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
        // The finalizer claimed the reservation, so its drop refunds nothing.
        tokio::task::yield_now().await;

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
    async fn native_terminal_event_is_held_until_the_request_is_settled() {
        let mut h = Harness::new("fin-native-hold");
        let event = |name: &str, usage: Option<Usage>| {
            let mut e = NativeResponsesEvent::new(Some(name.to_string()), "{}".to_string());
            e.usage = usage;
            Ok::<_, ProxyError>(e)
        };
        let inner = futures::stream::iter(vec![
            event("response.created", None),
            event("response.completed", Some(usage(3, 4))),
            // Anything after the terminal event must not overtake or replace
            // it: nothing more is read from the upstream.
            event("response.output_text.delta", None),
            Err(ProxyError::StreamError("reset".into())),
        ])
        .boxed();
        let budget = h.budget.clone();
        let mut out = Box::pin(h.take().wrap_stream(inner));

        let first = out.next().await.unwrap().unwrap();
        assert_eq!(first.event.as_deref(), Some("response.created"));
        assert!(budget.calls.lock().unwrap().is_empty());

        let last = out.next().await.unwrap().unwrap();
        assert_eq!(last.event.as_deref(), Some("response.completed"));
        assert_eq!(
            h.reconciles().len(),
            1,
            "reconciled before the terminal event is released"
        );
        assert!(out.next().await.is_none());
        let row = h.usage_rx.try_recv().expect("usage row");
        assert_eq!((row.prompt_tokens, row.completion_tokens), (3, 4));
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
        tokio::task::yield_now().await;

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
