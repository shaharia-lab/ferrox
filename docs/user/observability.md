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
  "completion_tokens": 120
}
```

---

## Prometheus metrics

Metrics are available at `GET /metrics`. All metric names are prefixed `ferrox_`.

### Request metrics

| Metric | Type | Labels | Description |
|---|---|---|---|
| `ferrox_requests_total` | Counter | `provider`, `model_alias`, `model_id`, `status`, `key_name` | Total requests dispatched |
| `ferrox_request_duration_seconds` | Histogram | `provider`, `model_alias`, `status` | End-to-end latency |
| `ferrox_ttfb_seconds` | Histogram | `provider`, `model_alias` | Time to first byte |
| `ferrox_tokens_total` | Counter | `provider`, `model_alias`, `key_name`, `type` | Tokens processed per client (`type`: `prompt` or `completion`) |
| `ferrox_active_streams` | Gauge | `provider`, `model_alias` | Active SSE connections |
| `ferrox_errors_total` | Counter | `provider`, `error_type` | Errors by type |

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
| `ferrox_circuit_breaker_state` | Gauge | `provider`, `model_alias` | State: `0`=closed, `1`=open, `2`=half-open |
| `ferrox_circuit_breaker_trips_total` | Counter | `provider` | Times a circuit transitioned to open |

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
