# Observability

Ferrox emits structured logs, Prometheus metrics, and OpenTelemetry traces.

## Logging

Log format and level are configured under `telemetry`:

```yaml
telemetry:
  log_level: "info"    # trace | debug | info | warn | error
  log_format: "json"   # json | text
```

Every completed request emits a structured log line at `info` level:

```json
{
  "timestamp": "2026-03-28T10:00:00Z",
  "level": "INFO",
  "message": "request_completed",
  "request_id": "550e8400-e29b-41d4-a716-446655440000",
  "key_name": "my-app",
  "model_alias": "claude-sonnet",
  "provider": "anthropic-primary",
  "model_id": "claude-sonnet-4-20250514",
  "streaming": false,
  "status": 200,
  "latency_ms": 843,
  "prompt_tokens": 45,
  "completion_tokens": 120,
  "cache_read_tokens": 3968,
  "cache_write_tokens": 100
}
```

`cache_read_tokens` and `cache_write_tokens` are prompt-cache counters. They are
**omitted entirely** when the provider reported no cache usage, so requests
against non-caching providers log exactly as before. Both counters are logged on
every completed request — streaming and non-streaming, on `request_completed`
(`/v1/chat/completions`), `responses_request_completed` (`/v1/responses`) and
`anthropic_request_completed` (`/anthropic/v1/messages`) alike.

The same two counters, with the same omit-when-zero rule, are included in the
`token_usage` webhook payload — see
[event_endpoints](configuration.md#event-payload).

### Classified requests

A request to a [classified alias](routing.md#classified-aliases) logs one extra
`info` line, `Classified request`, before it is dispatched:

```json
{
  "timestamp": "2026-10-06T10:00:00Z",
  "level": "INFO",
  "message": "Classified request",
  "request_id": "550e8400-e29b-41d4-a716-446655440000",
  "requested_alias": "auto",
  "served_alias": "claude-haiku",
  "reason": "classified",
  "tier": "simple",
  "confidence": 0.93,
  "probabilities": "[(\"simple\", 0.93), (\"complex\", 0.07)]",
  "classifier_model": "jev-1.13.0",
  "classifier_input_tokens": 31,
  "classifier_latency_ms": 84,
  "cached": false
}
```

| Field | Description |
|---|---|
| `requested_alias` | The classified alias the client asked for |
| `served_alias` | The tier's alias or the `fallback_alias` that serves the request |
| `reason` | One of the eight [reasons](routing.md#classified-aliases) |
| `tier` | The tier the classifier named, as it named it. With `reason` `shadow`, the tier that would have served the request |
| `confidence`, `probabilities` | The classifier's confidence in its answer and its per-tier probabilities, when the backend reports them |
| `classifier_model`, `classifier_input_tokens` | The classifier model that answered and the tokens it billed (`0` for a cached answer) |
| `classifier_latency_ms` | Time spent on classification, input extraction included |
| `cached` | `true` when the answer came from the decision cache and the classifier was not called |
| `error` | Why there is no answer, as a short message such as `classifier timed out` or `classifier circuit breaker is open` |

The answer fields (`tier`, `confidence`, `probabilities`, `classifier_model`,
`classifier_input_tokens`) are omitted when the classifier gave no answer, and
`error` is omitted when it gave one. The line never contains request text.

The completion line that follows (`request_completed` and its siblings) carries
the served alias in `model_alias`, exactly as for a request sent to that alias
directly.

The decision is also recorded with the request's usage: six optional fields
(`requested_model`, `routing_reason`, `classifier_confidence`,
`classifier_latency_ms`, `classifier_input_tokens`, `classifier_model`) in the
`token_usage` webhook payload and as nullable `usage_log` columns. See
[Classified requests](configuration.md#classified-requests) for their values.

---

## Prometheus metrics

Metrics are available at `GET /metrics` (the default path). All metric names are
prefixed `ferrox_`. The endpoint is a public, unauthenticated route.

Both the exposure and the path are configurable under `telemetry.metrics`:

```yaml
telemetry:
  metrics:
    enabled: true        # set false to not mount the endpoint at all
    path: /metrics       # serve at a custom path, e.g. /internal/metrics
```

When `enabled: false`, the route is not mounted and requests to it return `404`.
When `path` is set, metrics are served only at that path.

### Request metrics

| Metric | Type | Labels | Description |
|---|---|---|---|
| `ferrox_requests_total` | Counter | `provider`, `model_alias`, `model_id`, `status`, `key_name` | Total requests dispatched |
| `ferrox_request_duration_seconds` | Histogram | `provider`, `model_alias`, `status` | End-to-end latency |
| `ferrox_ttfb_seconds` | Histogram | `provider`, `model_alias` | Time to first byte |
| `ferrox_tokens_total` | Counter | `provider`, `model_alias`, `key_name`, `type` | Tokens processed per client (`type`: `prompt`, `completion`, `cache_read`, `cache_write`) |
| `ferrox_active_streams` | Gauge | `provider`, `model_alias` | Active SSE connections |
| `ferrox_errors_total` | Counter | `provider`, `error_type` | Errors by type |

### Prompt-cache metrics

Providers that support prompt caching report two extra `type` values on
`ferrox_tokens_total`:

| `type` | Meaning |
|---|---|
| `cache_read` | Tokens served from the prompt cache (Anthropic `cache_read_input_tokens`, Bedrock `cacheReadInputTokens`, Gemini `cachedContentTokenCount`) |
| `cache_write` | Tokens written to the prompt cache (Anthropic `cache_creation_input_tokens`, Bedrock `cacheWriteInputTokens`) |

These series are created **only when a provider actually reports cache usage**,
so enabling this costs no cardinality on non-caching routes.

`prompt` counts the **non-cached** input tokens for a request — cache reads are
counted separately, not folded into `prompt`. Cache hit rate is therefore the
share of total input that was served from cache:

```promql
sum(rate(ferrox_tokens_total{type="cache_read"}[5m]))
/
sum(rate(ferrox_tokens_total{type=~"prompt|cache_read"}[5m]))
```

Break it down per model, client or provider by adding a `by` clause — e.g.
`by (model_alias)` on both halves. A sudden drop in this ratio is the signal
that caching has regressed (a changed system prompt, a lost cache breakpoint, or
a failover to a provider that does not cache).

### Routing metrics

| Metric | Type | Labels | Description |
|---|---|---|---|
| `ferrox_routing_target_selected` | Counter | `model_alias`, `provider`, `strategy` | Load balancer selections |
| `ferrox_fallback_total` | Counter | `model_alias`, `from_provider`, `to_provider` | Fallback activations |
| `ferrox_retries_total` | Counter | `provider`, `model_alias` | Retry attempts |
| `ferrox_rate_limited_total` | Counter | `key_name` | Requests rejected by rate limiter |

### Circuit breaker metrics

| Metric | Type | Labels | Description |
|---|---|---|---|
| `ferrox_circuit_breaker_state` | Gauge | `provider`, `model_alias` | State: `0`=closed, `1`=open, `2`=half-open. A classifier's breaker reports `provider="classifier:<id>"` with an empty `model_alias` |
| `ferrox_circuit_breaker_trips_total` | Counter | `provider` | Times a circuit transitioned to open |

### Classifier metrics

These exist only for [classified aliases](routing.md#classified-aliases).

| Metric | Type | Labels | Description |
|---|---|---|---|
| `ferrox_classifier_decisions_total` | Counter | `alias`, `tier`, `reason` | Routing decisions for requests to a classified alias |
| `ferrox_classifier_duration_seconds` | Histogram | `classifier` | Time spent waiting for a classifier's answer, timeouts included |
| `ferrox_classifier_cache_hits_total` | Counter | `classifier` | Classified requests routed by a cached classifier answer, without a classifier call |
| `ferrox_classifier_cache_misses_total` | Counter | `classifier` | Classified requests whose input had no cached answer |

- `alias` is the classified alias the client asked for, not the alias that served the request.
- `tier` is the configured tier the classifier named, or `none` when there is no such tier: the classifier gave no answer, was not called, or named a tier the alias does not list. With `reason="shadow"` or `reason="low_confidence"` it is the tier that was named but not served.
- `reason` is one of `classified`, `low_confidence`, `timeout`, `error`, `unknown_choice`, `breaker_open`, `shadow`, `opt_out`; see [the table of reasons](routing.md#classified-aliases). Every reason other than `classified` was served by `fallback_alias`. A decision made from a cached answer is counted like any other.
- `classifier` is the `classifiers[].id`.

`ferrox_classifier_duration_seconds` has buckets from 10 ms to 5 s. Only a real call is timed: a cached answer, an opted-out request, an open breaker and a request with no user text add no sample. Neither cache counter moves for a classifier whose cache is disabled.

**Fallback rate** of a classified alias, the share of its requests the classifier did not route:

```promql
sum by (alias) (rate(ferrox_classifier_decisions_total{reason!="classified"}[5m]))
/
sum by (alias) (rate(ferrox_classifier_decisions_total[5m]))
```

On an alias in shadow mode every request is a fallback, so graph `reason="shadow"` there instead: that is the share of requests going live would reroute.

**p95 classifier latency**:

```promql
histogram_quantile(0.95, sum by (le, classifier) (rate(ferrox_classifier_duration_seconds_bucket[5m])))
```

A classifier that is down shows up as `ferrox_circuit_breaker_state{provider="classifier:<id>"} == 1` (see [Circuit breaker metrics](#circuit-breaker-metrics)) and as a rising `reason="breaker_open"` rate.

The request, token and latency metrics of a classified request are recorded under the alias that served it (`model_alias` is the tier's alias or the fallback's).

### Webhook metrics

| Metric | Type | Labels | Description |
|---|---|---|---|
| `ferrox_webhook_dispatched_total` | Counter | `endpoint` | Webhook events successfully delivered |
| `ferrox_webhook_errors_total` | Counter | `endpoint` | Delivery failures after all retries exhausted |

---

## Prometheus scrape config

```yaml
scrape_configs:
  - job_name: ferrox
    static_configs:
      - targets:
          - ferrox:8080
    metrics_path: /metrics
    scrape_interval: 10s
```

---

## OpenTelemetry tracing

Enable distributed tracing by configuring the OTLP exporter:

```yaml
telemetry:
  tracing:
    enabled: true
    otlp_endpoint: "http://otel-collector:4317"
    service_name: "ferrox"
    service_version: "0.1.0"
    sample_rate: 0.1    # sample 10% of traces in production
```

Spans are exported via gRPC to the configured OTLP endpoint. Use `docker compose up` to start a local Jaeger instance for development.

### Sample rates

| Environment | Recommended `sample_rate` |
|---|---|
| Development | `1.0` (all traces) |
| Staging | `0.5` |
| Production | `0.05` to `0.1` |

---

## Docker Compose observability stack

Running `docker compose up` starts the full stack:

| Service | URL | Purpose |
|---|---|---|
| Ferrox | `:8080` | Proxy |
| Grafana | `:3000` | Dashboards, metrics, traces (admin/admin) |
| OTLP gRPC | `:4317` | Trace/metric ingestion |
| OTLP HTTP | `:4318` | Trace/metric ingestion |

The `grafana/otel-lgtm` image bundles Grafana, Loki, Tempo, Mimir, and the OTEL Collector — no extra services needed.
