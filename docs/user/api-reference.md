# API Reference

Ferrox exposes two API surfaces on the same port:

| Surface | Path prefix | For |
|---|---|---|
| OpenAI-compatible | `/v1/` | OpenAI SDK, Codex CLI, Aider, Cursor, Cline, etc. |
| Anthropic-native | `/anthropic/v1/` | Anthropic SDK, Claude Code CLI |

Both surfaces route through the same `ModelRouter`, so every configured model alias is accessible from either endpoint.

## Base URLs

```
http://your-ferrox-host:8080          # OpenAI SDK base URL
http://your-ferrox-host:8080/anthropic # Anthropic SDK base URL
```

## Authentication

**OpenAI-compatible endpoints** (`/v1/*`) accept a Bearer token:

```
Authorization: Bearer <virtual-key>
```

**Anthropic-native endpoints** (`/anthropic/v1/*`) accept either header — the `x-api-key` header is checked first (Anthropic SDK default), then `Authorization: Bearer` as a fallback:

```
x-api-key: <virtual-key>
```

Health and metrics endpoints are public.

---

## POST /v1/chat/completions

Send a chat completion request. Ferrox routes it to the configured provider based on the `model` field.

### Request

```json
{
  "model": "claude-sonnet",
  "messages": [
    {"role": "system", "content": "You are a helpful assistant."},
    {"role": "user", "content": "Hello"}
  ],
  "stream": false,
  "temperature": 0.7,
  "max_tokens": 1024,
  "top_p": 1.0,
  "stop": ["END"],
  "tools": [ ... ],
  "tool_choice": "auto"
}
```

| Field | Type | Required | Description |
|---|---|---|---|
| `model` | string | yes | Model alias from your config |
| `messages` | array | yes | Conversation history |
| `stream` | boolean | no | Enable SSE streaming (default: false) |
| `temperature` | float | no | Sampling temperature |
| `max_tokens` | integer | no | Max tokens to generate |
| `top_p` | float | no | Nucleus sampling |
| `stop` | string or array | no | Stop sequences |
| `tools` | array | no | Tool definitions |
| `tool_choice` | string or object | no | Tool selection mode |

Unknown fields are forwarded to the provider as-is.

### Non-streaming response

```json
{
  "id": "chatcmpl-abc123",
  "object": "chat.completion",
  "created": 1735000000,
  "model": "claude-sonnet",
  "choices": [
    {
      "index": 0,
      "message": {
        "role": "assistant",
        "content": "Hello! How can I help you today?"
      },
      "finish_reason": "stop"
    }
  ],
  "usage": {
    "prompt_tokens": 15,
    "completion_tokens": 12,
    "total_tokens": 27
  }
}
```

### Streaming response

When `stream: true`, responses are sent as Server-Sent Events:

```
data: {"id":"chatcmpl-abc","object":"chat.completion.chunk","created":1735000000,"model":"claude-sonnet","choices":[{"index":0,"delta":{"role":"assistant","content":"Hello"},"finish_reason":null}]}

data: {"id":"chatcmpl-abc","object":"chat.completion.chunk","created":1735000000,"model":"claude-sonnet","choices":[{"index":0,"delta":{"content":"!"},"finish_reason":null}]}

data: {"id":"chatcmpl-abc","object":"chat.completion.chunk","created":1735000000,"model":"claude-sonnet","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":15,"completion_tokens":2,"total_tokens":17}}

data: [DONE]
```

### Error responses

All errors use OpenAI error format:

```json
{
  "error": {
    "message": "Key 'my-app' is not authorized to use model 'gpt-4o'",
    "type": "forbidden",
    "code": 403
  }
}
```

| Status | Type | Cause |
|---|---|---|
| 401 | `unauthorized` | Missing or invalid API key |
| 403 | `forbidden` | Key not allowed to use this model |
| 404 | `model_not_found` | Model alias not in config |
| 429 | `rate_limited` | Per-key rate limit exceeded |
| 429 | `budget_exceeded` | Client's token budget exhausted |
| 500 | `stream_error` | Upstream streaming failure |
| 502 | `circuit_open` | Circuit breaker open; all targets unavailable |
| 502 | `provider_error` | Provider returned an error |
| 504 | `upstream_timeout` | Provider did not respond in time |

---

## POST /v1/responses

The OpenAI **Responses API** (`client.responses.create(...)`), used by Codex CLI, the OpenAI Agents SDK and newer OpenAI SDK code. Ferrox translates the request to its internal chat format, routes it exactly like `/v1/chat/completions` (same aliases, retries, failover, circuit breakers, virtual-key auth, rate limits, budgets, `usage_log` rows and `token_usage` webhooks), and encodes the answer back as a Responses object. It therefore works with **every** configured provider, not only OpenAI.

The endpoint is **stateless**: nothing is stored, `store` is accepted and always echoed as `false` (except on a [native](#native-passthrough) alias), and the client sends the full conversation in `input` on every turn (which is what Codex CLI does).

### Request

```json
{
  "model": "claude-sonnet",
  "instructions": "You are a helpful assistant.",
  "input": [
    {"role": "user", "content": "What's the weather in Paris?"}
  ],
  "tools": [
    {"type": "function", "name": "get_weather",
     "parameters": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]}}
  ],
  "max_output_tokens": 1024,
  "stream": false
}
```

**Supported:**

| Field | Notes |
|---|---|
| `model`, `instructions`, `max_output_tokens`, `temperature`, `top_p`, `stream` | `instructions` becomes a leading system message |
| `input` | A string, or a list of `message` (roles `user` / `assistant` / `system` / `developer`), `function_call`, `function_call_output`, `custom_tool_call`, `custom_tool_call_output` and `reasoning` items. Content parts: `input_text`, `output_text`, `refusal`, `input_image` (with `image_url`) |
| `tools` | `function` and `custom` (free-form text input) tools |
| `tool_choice`, `parallel_tool_calls`, `max_tool_calls` | |
| `text.format` | `text`, `json_object`, `json_schema` (mapped to the chat `response_format`; schema adherence depends on the upstream model) |
| `reasoning.effort` | Forwarded as `reasoning_effort`. On an Anthropic target it turns on extended thinking for the model (adaptive + `output_config.effort` on Claude 4.6+, a `budget_tokens` below `max_tokens` on Claude 3.7–4.5; nothing on other models) and drops `temperature` and a `top_p` below 0.95 and downgrades a forced `tool_choice` to `auto`; `none` leaves thinking off |
| `reasoning.summary`, `text.verbosity`, `include`, `truncation` | Forwarded as hints |
| `metadata`, `prompt_cache_key`, `safety_identifier`, `user`, `service_tier` | Passed through |

Unknown top-level fields are ignored, so a newer SDK still works.

**Rejected with a 400** (`invalid_request_error`, with `param` naming the field; a [native](#native-passthrough) target accepts the last two rows):

| Field | Why |
|---|---|
| `previous_response_id`, `conversation`, `prompt` | Need server-side state — send the full conversation in `input` instead |
| `background: true` | Needs a server-side job store |
| Hosted built-in tools (`web_search*`, `file_search`, `code_interpreter`, `computer*`, `mcp`, `image_generation`, `shell`, `local_shell`, `apply_patch`, `tool_search`, `namespace`) | Need server-side execution; declare them as `function` tools instead |
| `input_image` with only a `file_id`, `input_file` content | Need the OpenAI Files API |

`GET` / `DELETE /v1/responses/{id}`, `input_items`, `cancel`, `input_tokens`, `compact` and WebSocket mode are not implemented.

### Native passthrough

A `type: openai` provider configured with `responses: native` receives the client's body unchanged except for `model`, at `{base_url}/responses`, and its `response` object or event stream is passed through verbatim (ids and `model` are the upstream's). Accounting is the same as above. On such a target, the hosted tools and Files-API inputs in the table above are **not** rejected; on an alias with a mix of native and translate-only targets, requests using them go only to the native ones (or get the `400` when none can take them). `store: true` is rejected with a `400` on any alias with a native target. See [providers](providers.md#native-responses-api-passthrough).

### Non-streaming response

```json
{
  "id": "resp_2f4c9e0b7a1d4e53b8c6f1a0d2e3b4c5",
  "object": "response",
  "created_at": 1735000000,
  "status": "completed",
  "model": "claude-sonnet",
  "output": [
    {
      "type": "message",
      "id": "msg_2f4c9e0b7a1d4e53b8c6f1a0d2e3b4c5_0",
      "role": "assistant",
      "status": "completed",
      "content": [{"type": "output_text", "text": "Hello!", "annotations": [], "logprobs": []}]
    }
  ],
  "usage": {
    "input_tokens": 15,
    "input_tokens_details": {"cached_tokens": 0, "cache_write_tokens": 0},
    "output_tokens": 2,
    "output_tokens_details": {"reasoning_tokens": 0},
    "total_tokens": 17
  },
  "store": false
}
```

The request parameters (`instructions`, `tools`, `tool_choice`, `temperature`, `max_output_tokens`, …) are echoed back as the OpenAI API does. `output` holds a `reasoning` item when the model returned reasoning, a `message` item for text, and one `function_call` (or `custom_tool_call`) item per tool call. A `length` stop becomes `status: "incomplete"` with `incomplete_details.reason: "max_output_tokens"`.

### Streaming response

When `stream: true`, the response is an SSE stream of typed events. Each frame's `event:` name equals its `type`, every event carries a monotonic `sequence_number`, and there is **no** `[DONE]` sentinel:

```
event: response.created
data: {"type":"response.created","sequence_number":0,"response":{"id":"resp_…","object":"response","status":"in_progress",…}}

event: response.in_progress
data: {"type":"response.in_progress","sequence_number":1,"response":{…}}

event: response.output_item.added
data: {"type":"response.output_item.added","sequence_number":2,"output_index":0,"item":{"type":"message",…}}

event: response.output_text.delta
data: {"type":"response.output_text.delta","sequence_number":4,"item_id":"msg_…_0","output_index":0,"content_index":0,"delta":"Hello"}

…

event: response.completed
data: {"type":"response.completed","sequence_number":9,"response":{"id":"resp_…","status":"completed","output":[…],"usage":{"input_tokens":15,"output_tokens":2,…}}}
```

The stream ends with exactly one of `response.completed`, `response.incomplete` or `response.failed`; usage is reported only in that terminal event. An upstream error after the stream started arrives in-band as `response.failed`.

### Error responses

Errors that happen before the response starts use the same OpenAI error format and status codes as [`/v1/chat/completions`](#error-responses). Translation failures are `400 invalid_request_error` with a `param`:

```json
{
  "error": {
    "message": "`previous_response_id` is not supported: this endpoint is stateless — send the full conversation in `input`",
    "type": "invalid_request_error",
    "code": 400,
    "param": "previous_response_id"
  }
}
```

---

## GET /v1/models

List all configured model aliases.

### Response

```json
{
  "object": "list",
  "data": [
    {
      "id": "claude-sonnet",
      "object": "model",
      "created": 1735000000,
      "owned_by": "ferrox"
    },
    {
      "id": "gpt-4o",
      "object": "model",
      "created": 1735000000,
      "owned_by": "ferrox"
    }
  ]
}
```

---

---

## POST /anthropic/v1/messages

Send a chat request using the Anthropic Messages API format. Ferrox translates it internally and routes it through the same `ModelRouter` as `/v1/chat/completions`, so **any configured model alias works** — not just Anthropic/Claude models.

Requires `x-api-key: <virtual-key>` (or `Authorization: Bearer <virtual-key>`).

### Request

```json
{
  "model": "claude-sonnet",
  "max_tokens": 1024,
  "system": "You are a helpful assistant.",
  "messages": [
    {"role": "user", "content": "Hello"}
  ],
  "stream": false,
  "temperature": 0.7,
  "top_p": 1.0,
  "stop_sequences": ["END"]
}
```

| Field | Type | Required | Description |
|---|---|---|---|
| `model` | string | yes | Model alias from your config |
| `messages` | array | yes | Conversation history |
| `max_tokens` | integer | yes | Max tokens to generate |
| `system` | string | no | System prompt |
| `stream` | boolean | no | Enable SSE streaming (default: false) |
| `temperature` | float | no | Sampling temperature |
| `top_p` | float | no | Nucleus sampling |
| `stop_sequences` | array | no | Stop sequences |
| `metadata` | object | no | Accepted but not forwarded |
| `top_k` | integer | no | Accepted but not forwarded |

### Non-streaming response

```json
{
  "id": "msg_abc123",
  "type": "message",
  "role": "assistant",
  "model": "claude-sonnet",
  "content": [
    {"type": "text", "text": "Hello! How can I help you?"}
  ],
  "stop_reason": "end_turn",
  "stop_sequence": null,
  "usage": {
    "input_tokens": 15,
    "output_tokens": 10
  }
}
```

### Streaming response

When `stream: true`, responses use the Anthropic SSE event protocol:

```
event: message_start
data: {"type":"message_start","message":{"id":"msg_abc","type":"message","role":"assistant","content":[],"model":"claude-sonnet","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":15,"output_tokens":1}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: ping
data: {"type":"ping"}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"input_tokens":15,"output_tokens":10}}

event: message_stop
data: {"type":"message_stop"}
```

### Prompt-cache token counters

When an upstream provider reports prompt-cache usage, Ferrox passes the counters
through on both API surfaces. They are **omitted entirely** when the upstream
does not report them, so responses from non-caching providers are unchanged.

On `/anthropic/v1/messages` they appear under their native names, in both the
non-streaming `usage` object and the streaming `message_delta`:

```json
"usage": {
  "input_tokens": 47,
  "output_tokens": 2,
  "cache_creation_input_tokens": 100,
  "cache_read_input_tokens": 3968
}
```

On `/v1/chat/completions` both counters appear in OpenAI's canonical form under
`prompt_tokens_details`, and the native Anthropic keys are preserved alongside
them:

```json
"usage": {
  "prompt_tokens": 47,
  "completion_tokens": 2,
  "total_tokens": 49,
  "prompt_tokens_details": {"cached_tokens": 3968, "cache_write_tokens": 100},
  "cache_read_input_tokens": 3968,
  "cache_creation_input_tokens": 100
}
```

> `prompt_tokens_details.cached_tokens` and `cache_read_input_tokens` describe
> **the same tokens** in two vocabularies, as do
> `prompt_tokens_details.cache_write_tokens` and `cache_creation_input_tokens`.
> Read one of each pair — never sum them.

The nested pair is what the official OpenAI SDKs expose
(`PromptTokensDetails.cached_tokens` / `.cache_write_tokens` in both
`openai-python` and `openai-go`); the top-level Anthropic-native keys are an
extension those SDKs ignore, so either vocabulary works with a stock client.

Note that `input_tokens` / `prompt_tokens` counts the **non-cached** input for
that request; cached tokens are reported separately in the fields above.

### Stop reason mapping

| OpenAI `finish_reason` | Anthropic `stop_reason` |
|---|---|
| `stop` | `end_turn` |
| `length` | `max_tokens` |
| `tool_calls` | `tool_use` |

---

## GET /anthropic/v1/models

List all configured model aliases in Anthropic format.

Requires `x-api-key: <virtual-key>` (or `Authorization: Bearer <virtual-key>`).

### Response

```json
{
  "data": [
    {
      "type": "model",
      "id": "claude-sonnet",
      "display_name": "claude-sonnet",
      "created_at": "1970-01-01T00:00:00Z"
    },
    {
      "type": "model",
      "id": "gpt-4o",
      "display_name": "gpt-4o",
      "created_at": "1970-01-01T00:00:00Z"
    }
  ],
  "has_more": false,
  "first_id": "claude-sonnet",
  "last_id": "gpt-4o"
}
```

---

## GET /healthz

Liveness check. Always returns `200 OK` with body `ok` if the process is running.

---

## GET /readyz

Readiness check. Returns `200 OK` with body `ready` when the server has finished startup. Returns `503 Service Unavailable` during startup or graceful shutdown drain.

Use `/readyz` for readiness probes and load balancer health checks.

---

## GET /metrics

Prometheus metrics in text exposition format (content type `text/plain; version=0.0.4`).

No authentication required. See [Observability](observability.md) for the full metric list.

---

## GET /api-schema · GET /openapi.json

OpenAPI 3.x document describing the gateway's HTTP surface (content type `application/json`). Both paths serve the same document — `/openapi.json` is the convention most SDK generators and API clients auto-detect; `/api-schema` is a friendly alias.

No authentication required. Useful for gateway-only deployments (no control plane) to discover the API, generate typed SDKs, or import into Postman/Insomnia.

The Ferrox-owned shapes (`/v1/models`, `/anthropic/v1/models`, the error envelope) are modeled fully. The `/v1/chat/completions` and `/anthropic/v1/messages` bodies mirror the upstream OpenAI / Anthropic wire formats — only the fields Ferrox owns are modeled, with a reference to the upstream specs for the exhaustive tail.

```bash
curl http://localhost:8080/openapi.json | jq .
```

---

## Using the Anthropic SDK / Claude Code CLI

Point `ANTHROPIC_BASE_URL` at the `/anthropic` prefix. The SDK appends `/v1/messages` automatically.

**Claude Code CLI:**

```bash
export ANTHROPIC_BASE_URL=http://localhost:8080/anthropic
export ANTHROPIC_API_KEY=sk-proxy-key

claude --model gpt-4o        # routes to OpenAI via Ferrox
claude --model gemini-flash  # routes to Gemini via Ferrox
claude --model claude-sonnet # routes to Anthropic via Ferrox
```

**Python (Anthropic SDK):**

```python
import anthropic

client = anthropic.Anthropic(
    api_key="sk-proxy-key",
    base_url="http://localhost:8080/anthropic",
)

message = client.messages.create(
    model="gpt-4o",   # any Ferrox model alias
    max_tokens=1024,
    messages=[{"role": "user", "content": "Hello"}],
)
```

**Node.js (Anthropic SDK):**

```javascript
import Anthropic from "@anthropic-ai/sdk";

const client = new Anthropic({
  apiKey: "sk-proxy-key",
  baseURL: "http://localhost:8080/anthropic",
});

const msg = await client.messages.create({
  model: "gpt-4o",
  max_tokens: 1024,
  messages: [{ role: "user", content: "Hello" }],
});
```

---

## Using OpenAI SDKs

Point the base URL at Ferrox and use your virtual key:

**Python:**

```python
from openai import OpenAI

client = OpenAI(
    api_key="sk-proxy-key",
    base_url="http://localhost:8080/v1"
)

response = client.chat.completions.create(
    model="claude-sonnet",
    messages=[{"role": "user", "content": "Hello"}]
)
```

**Node.js:**

```javascript
import OpenAI from "openai";

const client = new OpenAI({
  apiKey: "sk-proxy-key",
  baseURL: "http://localhost:8080/v1",
});

const response = await client.chat.completions.create({
  model: "claude-sonnet",
  messages: [{ role: "user", content: "Hello" }],
});
```

**Responses API** (`POST /v1/responses`) — the same clients, any configured alias:

```python
response = client.responses.create(model="claude-sonnet", input="Hello")
print(response.output_text, response.usage.input_tokens, response.usage.output_tokens)

# Streaming: typed events, ending in response.completed
for event in client.responses.create(model="claude-sonnet", input="Hello", stream=True):
    if event.type == "response.output_text.delta":
        print(event.delta, end="")
```

```javascript
const response = await client.responses.create({ model: "claude-sonnet", input: "Hello" });
console.log(response.output_text);
```

```go
client := openai.NewClient(option.WithBaseURL("http://localhost:8080/v1/"), option.WithAPIKey("sk-proxy-key"))
resp, err := client.Responses.New(ctx, responses.ResponseNewParams{
	Model: "claude-sonnet",
	Input: responses.ResponseNewParamsInputUnion{OfString: openai.String("Hello")},
})
```

For Codex CLI, see [Codex CLI](codex-cli.md).

---

## Control Plane API (ferrox-cp)

The control plane runs on port 9090 and manages clients, signing keys, and usage data. All admin endpoints require `Authorization: Bearer <CP_ADMIN_KEY>`.

### GET /api-schema · GET /openapi.json

OpenAPI 3.x document describing the control plane's full REST API — every `/api/*` admin route plus `POST /token`, `GET /.well-known/jwks.json` and `GET /healthz` — with request/response schemas and per-route auth (`admin_auth` for `CP_ADMIN_KEY`, `client_key_auth` for the `sk-cp-` client key on `/token`). Content type `application/json`; both paths serve the same document.

No authentication required.

```bash
curl http://localhost:9090/openapi.json | jq .
```

### GET /api/clients/:id/usage

Returns aggregated token usage for a client over the last 24h, 7d, and 30d.

```json
{
  "last_24h": { "total_prompt_tokens": 1200, "total_completion_tokens": 800, "total_tokens": 2000, "request_count": 15 },
  "last_7d":  { "total_prompt_tokens": 8000, "total_completion_tokens": 4000, "total_tokens": 12000, "request_count": 95 },
  "last_30d": { "total_prompt_tokens": 30000, "total_completion_tokens": 15000, "total_tokens": 45000, "request_count": 350 }
}
```

### GET /api/clients/:id/usage/details

Returns paginated per-request usage records.

| Param | Type | Default | Description |
|---|---|---|---|
| `from` | ISO 8601 timestamp | — | Filter: created_at >= from |
| `to` | ISO 8601 timestamp | — | Filter: created_at < to |
| `model` | string | — | Filter by model alias |
| `limit` | integer | 50 | Max records per page (max 1000) |
| `offset` | integer | 0 | Skip records for pagination |

```json
[
  {
    "request_id": "550e8400-e29b-41d4-a716-446655440000",
    "model": "claude-sonnet",
    "provider": "anthropic-primary",
    "prompt_tokens": 120,
    "completion_tokens": 80,
    "total_tokens": 200,
    "latency_ms": 843,
    "created_at": "2026-04-06T10:30:00Z"
  }
]
```

### PATCH /api/clients/:id/budget

Update token budget settings for a client. Both fields must be set together, or both null to remove the budget.

```json
{ "token_budget": 500000, "budget_period": "monthly" }
```

Returns the updated client object. `budget_period` must be `"daily"` or `"monthly"`.

### POST /api/clients/:id/reactivate

Re-activate a revoked client and reset its budget period. Returns `204 No Content`.

Use this after a client was revoked for exceeding its token budget. The budget counter resets to zero for the new period.
