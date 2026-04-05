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
| 500 | `stream_error` | Upstream streaming failure |
| 502 | `circuit_open` | Circuit breaker open; all targets unavailable |
| 502 | `provider_error` | Provider returned an error |
| 504 | `upstream_timeout` | Provider did not respond in time |

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
data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":10}}

event: message_stop
data: {"type":"message_stop"}
```

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
