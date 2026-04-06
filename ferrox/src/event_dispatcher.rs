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

async fn dispatch_loop(
    endpoints: Vec<EventEndpointConfig>,
    mut rx: mpsc::Receiver<TokenUsageEvent>,
) {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .build()
        .expect("failed to create webhook HTTP client");

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

            // Spawn a per-endpoint delivery task so slow endpoints don't block others.
            tokio::spawn(async move {
                deliver_with_retry(&client, &ep_name, &ep_url, &ep_token, &payload).await;
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

    #[tokio::test]
    async fn noop_dispatcher_does_not_panic() {
        let dispatcher = noop_dispatcher();
        dispatcher.dispatch(TokenUsageEvent {
            event: "token_usage",
            request_id: "test".to_string(),
            client_id: Some(Uuid::new_v4()),
            key_name: "test-key".to_string(),
            model: "gpt-4".to_string(),
            provider: "openai".to_string(),
            prompt_tokens: 100,
            completion_tokens: 50,
            total_tokens: 150,
            latency_ms: Some(200),
            timestamp: Utc::now(),
        });
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
}
