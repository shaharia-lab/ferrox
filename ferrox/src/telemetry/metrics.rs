use once_cell::sync::Lazy;
use prometheus::{
    register_counter_vec, register_gauge_vec, register_histogram_vec, CounterVec, GaugeVec,
    HistogramVec, TextEncoder,
};

// ── Request counters / histograms ─────────────────────────────────────────────

/// Total requests dispatched.
/// Labels: provider, model_alias, model_id, status (HTTP code as string), key_name
pub static REQUESTS_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    register_counter_vec!(
        "ferrox_requests_total",
        "Total number of requests dispatched",
        &["provider", "model_alias", "model_id", "status", "key_name"]
    )
    .expect("register ferrox_requests_total")
});

/// End-to-end request latency in seconds.
/// Labels: provider, model_alias, status
pub static REQUEST_DURATION_SECONDS: Lazy<HistogramVec> = Lazy::new(|| {
    register_histogram_vec!(
        "ferrox_request_duration_seconds",
        "End-to-end request latency in seconds",
        &["provider", "model_alias", "status"],
        vec![0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0]
    )
    .expect("register ferrox_request_duration_seconds")
});

/// Time to first byte / first SSE chunk from provider.
/// Labels: provider, model_alias
pub static TTFB_SECONDS: Lazy<HistogramVec> = Lazy::new(|| {
    register_histogram_vec!(
        "ferrox_ttfb_seconds",
        "Time to first byte from provider in seconds",
        &["provider", "model_alias"],
        vec![0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0]
    )
    .expect("register ferrox_ttfb_seconds")
});

/// Token counts.
/// Labels: provider, model_alias, key_name, type (prompt|completion)
///
/// The `key_name` label enables per-client token usage dashboards and alerting.
/// Cardinality note: this is safe when the number of API clients is bounded
/// (typical for an API gateway). If unbounded client creation is expected,
/// consider dropping this label or using a recording rule.
pub static TOKENS_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    register_counter_vec!(
        "ferrox_tokens_total",
        "Total tokens processed",
        &["provider", "model_alias", "key_name", "type"]
    )
    .expect("register ferrox_tokens_total")
});

/// Currently active SSE streaming connections.
/// Labels: provider, model_alias
pub static ACTIVE_STREAMS: Lazy<GaugeVec> = Lazy::new(|| {
    register_gauge_vec!(
        "ferrox_active_streams",
        "Active SSE streaming connections",
        &["provider", "model_alias"]
    )
    .expect("register ferrox_active_streams")
});

/// Errors by type.
/// Labels: provider, error_type
pub static ERRORS_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    register_counter_vec!(
        "ferrox_errors_total",
        "Errors by type",
        &["provider", "error_type"]
    )
    .expect("register ferrox_errors_total")
});

// ── Circuit breaker ───────────────────────────────────────────────────────────

/// Current circuit breaker state (0=closed, 1=open, 2=half_open).
/// Labels: provider, model_alias
pub static CIRCUIT_BREAKER_STATE: Lazy<GaugeVec> = Lazy::new(|| {
    register_gauge_vec!(
        "ferrox_circuit_breaker_state",
        "Circuit breaker state: 0=closed, 1=open, 2=half_open",
        &["provider", "model_alias"]
    )
    .expect("register ferrox_circuit_breaker_state")
});

/// Total times a circuit breaker transitioned to Open.
/// Labels: provider
pub static CIRCUIT_BREAKER_TRIPS_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    register_counter_vec!(
        "ferrox_circuit_breaker_trips_total",
        "Number of times a circuit breaker opened",
        &["provider"]
    )
    .expect("register ferrox_circuit_breaker_trips_total")
});

// ── Routing ───────────────────────────────────────────────────────────────────

/// Fallback activations.
/// Labels: model_alias, from_provider, to_provider
pub static FALLBACK_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    register_counter_vec!(
        "ferrox_fallback_total",
        "Fallback activations",
        &["model_alias", "from_provider", "to_provider"]
    )
    .expect("register ferrox_fallback_total")
});

/// Retry attempts.
/// Labels: provider, model_alias
pub static RETRIES_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    register_counter_vec!(
        "ferrox_retries_total",
        "Retry attempts",
        &["provider", "model_alias"]
    )
    .expect("register ferrox_retries_total")
});

/// Rate-limited requests (per key name — backwards-compatible label).
/// Labels: key_name
pub static RATE_LIMITED_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    register_counter_vec!(
        "ferrox_rate_limited_total",
        "Requests rejected by the per-key rate limiter",
        &["key_name"]
    )
    .expect("register ferrox_rate_limited_total")
});

/// Requests allowed by the rate limiter, by backend.
/// Labels: backend (memory | redis)
pub static RATELIMIT_ALLOWED_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    register_counter_vec!(
        "ferrox_ratelimit_allowed_total",
        "Requests allowed by the rate limit backend",
        &["backend"]
    )
    .expect("register ferrox_ratelimit_allowed_total")
});

/// Requests denied by the rate limiter, by backend.
/// Labels: backend (memory | redis)
pub static RATELIMIT_DENIED_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    register_counter_vec!(
        "ferrox_ratelimit_denied_total",
        "Requests denied by the rate limit backend",
        &["backend"]
    )
    .expect("register ferrox_ratelimit_denied_total")
});

/// Errors contacting the rate limit backend (Redis unavailable, script error, etc).
/// Labels: backend (redis)
pub static RATELIMIT_BACKEND_ERRORS_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    register_counter_vec!(
        "ferrox_ratelimit_backend_errors_total",
        "Errors contacting the rate limit backend",
        &["backend"]
    )
    .expect("register ferrox_ratelimit_backend_errors_total")
});

/// Which target was selected by the load balancer.
/// Labels: model_alias, provider, strategy
pub static ROUTING_TARGET_SELECTED: Lazy<CounterVec> = Lazy::new(|| {
    register_counter_vec!(
        "ferrox_routing_target_selected",
        "Load balancer target selections",
        &["model_alias", "provider", "strategy"]
    )
    .expect("register ferrox_routing_target_selected")
});

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Render all registered metrics in Prometheus text format.
pub fn gather() -> String {
    // Force registration of all statics on first call
    Lazy::force(&REQUESTS_TOTAL);
    Lazy::force(&REQUEST_DURATION_SECONDS);
    Lazy::force(&TTFB_SECONDS);
    Lazy::force(&TOKENS_TOTAL);
    Lazy::force(&ACTIVE_STREAMS);
    Lazy::force(&ERRORS_TOTAL);
    Lazy::force(&CIRCUIT_BREAKER_STATE);
    Lazy::force(&CIRCUIT_BREAKER_TRIPS_TOTAL);
    Lazy::force(&FALLBACK_TOTAL);
    Lazy::force(&RETRIES_TOTAL);
    Lazy::force(&RATE_LIMITED_TOTAL);
    Lazy::force(&ROUTING_TARGET_SELECTED);
    Lazy::force(&RATELIMIT_ALLOWED_TOTAL);
    Lazy::force(&RATELIMIT_DENIED_TOTAL);
    Lazy::force(&RATELIMIT_BACKEND_ERRORS_TOTAL);

    let encoder = TextEncoder::new();
    let families = prometheus::gather();
    encoder.encode_to_string(&families).unwrap_or_default()
}

/// Record an observed token usage from a completed request.
pub fn record_tokens(
    provider: &str,
    model_alias: &str,
    key_name: &str,
    prompt: u32,
    completion: u32,
) {
    TOKENS_TOTAL
        .with_label_values(&[provider, model_alias, key_name, "prompt"])
        .inc_by(prompt as f64);
    TOKENS_TOTAL
        .with_label_values(&[provider, model_alias, key_name, "completion"])
        .inc_by(completion as f64);
}
