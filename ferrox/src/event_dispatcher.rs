use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::config::EventEndpointConfig;

/// A token usage event to be dispatched to webhook endpoints.
#[derive(Debug, Clone, Serialize)]
pub struct TokenUsageEvent {
    pub event: &'static str,
    pub request_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<Uuid>,
    pub key_name: String,
    pub model: String,
    pub provider: String,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    pub latency_ms: Option<u64>,
    pub timestamp: DateTime<Utc>,
}

/// Handle for dispatching events from request handlers.
///
/// Cheap to clone — each handler gets its own sender.
#[derive(Clone)]
pub struct EventDispatcher {
    tx: mpsc::Sender<TokenUsageEvent>,
}

impl EventDispatcher {
    /// Dispatch a token usage event.  Non-blocking — drops the event if the buffer is full.
    pub fn dispatch(&self, event: TokenUsageEvent) {
        if self.tx.try_send(event).is_err() {
            tracing::warn!("event dispatcher buffer full, dropping event");
            WEBHOOK_ERRORS_TOTAL
                .with_label_values(&["_buffer_full"])
                .inc();
        }
    }
}

/// Create a no-op dispatcher that silently discards all events.
/// Used when no event endpoints are configured.
pub fn noop_dispatcher() -> EventDispatcher {
    let (tx, mut rx) = mpsc::channel(1);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    EventDispatcher { tx }
}

/// Spawn the background dispatch task and return an `EventDispatcher` handle.
pub fn spawn_dispatcher(
    endpoints: Vec<EventEndpointConfig>,
    buffer_capacity: usize,
) -> EventDispatcher {
    let (tx, rx) = mpsc::channel(buffer_capacity);

    tokio::spawn(dispatch_loop(endpoints, rx));

    EventDispatcher { tx }
}

/// Maximum number of concurrent in-flight webhook delivery tasks.
/// Prevents unbounded task spawning when endpoints are slow or failing.
const MAX_INFLIGHT_DELIVERIES: usize = 256;

async fn dispatch_loop(
    endpoints: Vec<EventEndpointConfig>,
    mut rx: mpsc::Receiver<TokenUsageEvent>,
) {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .build()
        .expect("failed to create webhook HTTP client");

    let semaphore = Arc::new(tokio::sync::Semaphore::new(MAX_INFLIGHT_DELIVERIES));

    // Pre-filter endpoints that subscribe to token_usage events.
    let usage_endpoints: Vec<&EventEndpointConfig> = endpoints
        .iter()
        .filter(|ep| ep.events.iter().any(|e| e == "token_usage"))
        .collect();

    while let Some(event) = rx.recv().await {
        let payload = match serde_json::to_vec(&event) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(error = %e, "failed to serialize webhook event");
                continue;
            }
        };

        for ep in &usage_endpoints {
            let ep_name = ep.name.clone();
            let ep_url = ep.url.clone();
            let ep_token = ep.token.clone();
            let payload = payload.clone();
            let client = client.clone();
            let sem = semaphore.clone();

            // Acquire a permit before spawning — bounds concurrent tasks.
            // If all permits are taken, this awaits until one completes.
            let permit = match sem.acquire_owned().await {
                Ok(p) => p,
                Err(_) => break, // semaphore closed — shutting down
            };

            tokio::spawn(async move {
                deliver_with_retry(&client, &ep_name, &ep_url, &ep_token, &payload).await;
                drop(permit); // release the semaphore slot
            });
        }
    }

    tracing::info!("event dispatcher shutting down");
}

async fn deliver_with_retry(
    client: &reqwest::Client,
    name: &str,
    url: &str,
    token: &str,
    payload: &[u8],
) {
    const MAX_ATTEMPTS: u32 = 3;
    let backoffs = [1, 2, 4]; // seconds

    for attempt in 0..MAX_ATTEMPTS {
        let result = client
            .post(url)
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {}", token))
            .body(payload.to_vec())
            .send()
            .await;

        match result {
            Ok(resp) if resp.status().is_success() => {
                WEBHOOK_DISPATCHED_TOTAL.with_label_values(&[name]).inc();
                return;
            }
            Ok(resp) => {
                let status = resp.status();
                tracing::warn!(
                    endpoint = %name,
                    status = %status,
                    attempt = attempt + 1,
                    "webhook delivery failed with HTTP error"
                );
            }
            Err(e) => {
                tracing::warn!(
                    endpoint = %name,
                    error = %e,
                    attempt = attempt + 1,
                    "webhook delivery failed"
                );
            }
        }

        // Retry with backoff (except on last attempt).
        if attempt < MAX_ATTEMPTS - 1 {
            let delay = backoffs[attempt as usize];
            tokio::time::sleep(Duration::from_secs(delay)).await;
        }
    }

    // All attempts exhausted.
    tracing::error!(
        endpoint = %name,
        "webhook delivery failed after {} attempts, dropping event",
        MAX_ATTEMPTS
    );
    WEBHOOK_ERRORS_TOTAL.with_label_values(&[name]).inc();
}

// ── Prometheus metrics ──────────────────────────────────────────────────────

use once_cell::sync::Lazy;
use prometheus::{register_counter_vec, CounterVec};

static WEBHOOK_DISPATCHED_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    register_counter_vec!(
        "ferrox_webhook_dispatched_total",
        "Webhook events successfully delivered",
        &["endpoint"]
    )
    .expect("register ferrox_webhook_dispatched_total")
});

static WEBHOOK_ERRORS_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    register_counter_vec!(
        "ferrox_webhook_errors_total",
        "Webhook delivery failures (after all retries exhausted)",
        &["endpoint"]
    )
    .expect("register ferrox_webhook_errors_total")
});

/// Force-register the webhook metrics so they appear in /metrics output.
pub fn register_metrics() {
    Lazy::force(&WEBHOOK_DISPATCHED_TOTAL);
    Lazy::force(&WEBHOOK_ERRORS_TOTAL);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn sample_event() -> TokenUsageEvent {
        TokenUsageEvent {
            event: "token_usage",
            request_id: "req-123".to_string(),
            client_id: Some(Uuid::new_v4()),
            key_name: "test-key".to_string(),
            model: "gpt-4".to_string(),
            provider: "openai".to_string(),
            prompt_tokens: 100,
            completion_tokens: 50,
            total_tokens: 150,
            latency_ms: Some(200),
            timestamp: Utc::now(),
        }
    }

    #[tokio::test]
    async fn noop_dispatcher_does_not_panic() {
        let dispatcher = noop_dispatcher();
        dispatcher.dispatch(sample_event());
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    #[tokio::test]
    async fn token_usage_event_serializes_correctly() {
        let event = TokenUsageEvent {
            event: "token_usage",
            request_id: "req-123".to_string(),
            client_id: None,
            key_name: "my-app".to_string(),
            model: "claude-sonnet".to_string(),
            provider: "anthropic".to_string(),
            prompt_tokens: 120,
            completion_tokens: 80,
            total_tokens: 200,
            latency_ms: Some(843),
            timestamp: Utc::now(),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["event"], "token_usage");
        assert_eq!(json["total_tokens"], 200);
        assert_eq!(json["key_name"], "my-app");
        // client_id should be absent when None
        assert!(json.get("client_id").is_none());
    }

    #[tokio::test]
    async fn serialization_includes_client_id_when_present() {
        let cid = Uuid::new_v4();
        let event = TokenUsageEvent {
            client_id: Some(cid),
            ..sample_event()
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["client_id"].as_str().unwrap(), cid.to_string());
    }

    #[tokio::test]
    async fn serialization_includes_all_fields() {
        let event = sample_event();
        let json = serde_json::to_value(&event).unwrap();
        assert!(json.get("event").is_some());
        assert!(json.get("request_id").is_some());
        assert!(json.get("key_name").is_some());
        assert!(json.get("model").is_some());
        assert!(json.get("provider").is_some());
        assert!(json.get("prompt_tokens").is_some());
        assert!(json.get("completion_tokens").is_some());
        assert!(json.get("total_tokens").is_some());
        assert!(json.get("latency_ms").is_some());
        assert!(json.get("timestamp").is_some());
    }

    /// Start a minimal HTTP server that counts received requests and returns 200.
    async fn start_test_server(counter: Arc<AtomicU32>) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://127.0.0.1:{}/webhook", addr.port());

        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let counter = counter.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let _ = stream.read(&mut buf).await;
                    counter.fetch_add(1, Ordering::Relaxed);
                    let response = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n";
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });

        (url, handle)
    }

    /// Start a server that always returns 500 (to test retries).
    async fn start_failing_server(
        counter: Arc<AtomicU32>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://127.0.0.1:{}/webhook", addr.port());

        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let counter = counter.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let _ = stream.read(&mut buf).await;
                    counter.fetch_add(1, Ordering::Relaxed);
                    let response =
                        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n";
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });

        (url, handle)
    }

    #[tokio::test]
    async fn dispatcher_delivers_to_endpoint() {
        let counter = Arc::new(AtomicU32::new(0));
        let (url, _handle) = start_test_server(counter.clone()).await;

        let endpoints = vec![EventEndpointConfig {
            name: "test-ep".to_string(),
            url,
            token: "test-token".to_string(),
            events: vec!["token_usage".to_string()],
        }];

        let dispatcher = spawn_dispatcher(endpoints, 100);
        dispatcher.dispatch(sample_event());

        // Give the background task time to deliver.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(counter.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn dispatcher_delivers_to_multiple_endpoints() {
        let c1 = Arc::new(AtomicU32::new(0));
        let c2 = Arc::new(AtomicU32::new(0));
        let (url1, _h1) = start_test_server(c1.clone()).await;
        let (url2, _h2) = start_test_server(c2.clone()).await;

        let endpoints = vec![
            EventEndpointConfig {
                name: "ep-1".to_string(),
                url: url1,
                token: "t1".to_string(),
                events: vec!["token_usage".to_string()],
            },
            EventEndpointConfig {
                name: "ep-2".to_string(),
                url: url2,
                token: "t2".to_string(),
                events: vec!["token_usage".to_string()],
            },
        ];

        let dispatcher = spawn_dispatcher(endpoints, 100);
        dispatcher.dispatch(sample_event());

        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            c1.load(Ordering::Relaxed),
            1,
            "endpoint 1 should receive event"
        );
        assert_eq!(
            c2.load(Ordering::Relaxed),
            1,
            "endpoint 2 should receive event"
        );
    }

    #[tokio::test]
    async fn dispatcher_skips_endpoints_not_subscribed_to_event() {
        let counter = Arc::new(AtomicU32::new(0));
        let (url, _handle) = start_test_server(counter.clone()).await;

        let endpoints = vec![EventEndpointConfig {
            name: "wrong-events".to_string(),
            url,
            token: "t".to_string(),
            events: vec!["some_other_event".to_string()], // not token_usage
        }];

        let dispatcher = spawn_dispatcher(endpoints, 100);
        dispatcher.dispatch(sample_event());

        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            counter.load(Ordering::Relaxed),
            0,
            "endpoint should not receive events it didn't subscribe to"
        );
    }

    #[tokio::test]
    async fn dispatcher_retries_on_failure() {
        let counter = Arc::new(AtomicU32::new(0));
        let (url, _handle) = start_failing_server(counter.clone()).await;

        let endpoints = vec![EventEndpointConfig {
            name: "fail-ep".to_string(),
            url,
            token: "t".to_string(),
            events: vec!["token_usage".to_string()],
        }];

        let dispatcher = spawn_dispatcher(endpoints, 100);
        dispatcher.dispatch(sample_event());

        // Wait for all 3 retry attempts (1s + 2s + margin).
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert_eq!(
            counter.load(Ordering::Relaxed),
            3,
            "should attempt delivery 3 times before giving up"
        );
    }

    #[tokio::test]
    async fn dispatcher_drops_event_when_buffer_full() {
        // Create a dispatcher with buffer size 1, then fill it.
        let endpoints = vec![EventEndpointConfig {
            name: "slow-ep".to_string(),
            url: "http://127.0.0.1:1/unreachable".to_string(), // will hang on connect
            token: "t".to_string(),
            events: vec!["token_usage".to_string()],
        }];

        let dispatcher = spawn_dispatcher(endpoints, 1);

        // Fill the buffer — the first event may or may not get consumed by the
        // background task immediately, but sending many should eventually fill it.
        for _ in 0..10 {
            dispatcher.dispatch(sample_event());
        }

        // If we get here without blocking, the test passes — dispatch is non-blocking.
    }

    #[tokio::test]
    async fn dispatcher_delivers_multiple_events_sequentially() {
        let counter = Arc::new(AtomicU32::new(0));
        let (url, _handle) = start_test_server(counter.clone()).await;

        let endpoints = vec![EventEndpointConfig {
            name: "multi-ep".to_string(),
            url,
            token: "t".to_string(),
            events: vec!["token_usage".to_string()],
        }];

        let dispatcher = spawn_dispatcher(endpoints, 100);
        for _ in 0..5 {
            dispatcher.dispatch(sample_event());
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(counter.load(Ordering::Relaxed), 5);
    }
}
