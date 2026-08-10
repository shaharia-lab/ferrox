# Providers

Ferrox supports five provider types. Each can appear multiple times in the config with different API keys or base URLs.

## Anthropic

```yaml
providers:
  - name: anthropic-primary
    type: anthropic
    api_key: "${ANTHROPIC_API_KEY}"
    base_url: "https://api.anthropic.com"   # default; override for proxies
```

**Supported features:** chat completions, streaming, tool use, vision (image parts), system prompts.

Ferrox automatically handles the Anthropic Messages API format differences: it extracts the system prompt, converts content parts, and maps stop reasons.

**Required env var:** `ANTHROPIC_API_KEY`

---

## OpenAI

```yaml
providers:
  - name: openai-primary
    type: openai
    api_key: "${OPENAI_API_KEY}"
    base_url: "https://api.openai.com/v1"
```

Because Ferrox exposes an OpenAI-compatible API, requests are forwarded with minimal transformation. For streaming, Ferrox injects `stream_options: {include_usage: true}` to get token counts in the final chunk.

**Compatible with any OpenAI-compatible API** (Azure OpenAI, local models via LiteLLM, Ollama, etc.) by setting `base_url`.

> **`base_url` convention:** the value must include the API version segment.
> The adapter appends only `/chat/completions`, so the full request URL is
> `{base_url}/chat/completions`.  Examples:
> - OpenAI: `https://api.openai.com/v1`
> - Ollama: `http://localhost:11434/v1`
> - Azure OpenAI: `https://<resource>.openai.azure.com/openai/deployments/<deployment>/`

**Required env var:** `OPENAI_API_KEY`

---

## Gemini

```yaml
providers:
  - name: gemini-primary
    type: gemini
    api_key: "${GEMINI_API_KEY}"
    base_url: "https://generativelanguage.googleapis.com"
```

Ferrox maps the OpenAI message format to Gemini's `generateContent` / `streamGenerateContent` APIs. Role mapping: `assistant` becomes `model`, system messages become `systemInstruction`.

**Required env var:** `GEMINI_API_KEY`

---

## AWS Bedrock

Ferrox talks to Bedrock through the **Converse API**, so it supports **all
Bedrock model families** — Anthropic Claude, Amazon Nova/Titan, Meta Llama,
Mistral, Cohere, and others — not just Claude. Tool calling, multimodal image
inputs (see note below), and streaming all translate through the same path.

### Minimal (default credential chain)

```yaml
providers:
  - name: bedrock-us
    type: bedrock
    aws:
      region: "${AWS_REGION:-us-east-1}"
```

With no `aws.auth` block, credentials come from the standard AWS default chain:
environment variables (`AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` /
`AWS_SESSION_TOKEN`), `~/.aws/credentials` + `~/.aws/config` (incl. SSO), and
EC2/ECS/EKS instance roles / IRSA — the same resolution as the AWS CLI/SDKs.

### Authentication modes

Set **exactly one** base source under `aws.auth` (or omit `auth` for the default
chain). `assume_role` can layer an STS AssumeRole on top of any base source.
Always pass secrets via environment-variable interpolation — never inline them.

**Static credentials**

```yaml
    aws:
      region: us-east-1
      auth:
        access_key_id: "${AWS_ACCESS_KEY_ID}"
        secret_access_key: "${AWS_SECRET_ACCESS_KEY}"
        session_token: "${AWS_SESSION_TOKEN:-}"   # optional (temporary creds)
```

**Named profile / SSO**

```yaml
    aws:
      region: us-east-1
      auth:
        profile: "my-sso-profile"   # ~/.aws profile, incl. SSO / credential_process
```

**STS AssumeRole** (cross-account, least-privilege; temporary credentials are
auto-refreshed). Layers on top of the base source — static keys, a profile, or
the default chain when no base source is given:

```yaml
    aws:
      region: us-east-1
      auth:
        assume_role:
          role_arn: "arn:aws:iam::123456789012:role/ferrox"
          session_name: "ferrox"               # optional
          external_id: "${AWS_EXTERNAL_ID:-}"  # optional
          duration_secs: 3600                  # optional
```

### Model IDs and inference profiles

Use the Bedrock model ID as the `model_id` on a routing target. Many newer
models are only invocable through a **cross-region inference profile**, whose ID
is region-prefixed — pass that ID verbatim and Ferrox forwards it as-is:

```yaml
models:
  - alias: claude
    routing:
      strategy: failover
      targets:
        - provider: bedrock-us
          model_id: "eu.anthropic.claude-sonnet-4-5-20250929-v1:0"  # inference profile
        - provider: bedrock-us
          model_id: "amazon.nova-pro-v1:0"                          # on-demand model
```

**Notes**

- `endpoint_url` under `aws` overrides the Bedrock runtime endpoint (VPC
  endpoints / testing); rarely needed.
- Image inputs must be inline base64 `data:` URLs — Converse takes image
  **bytes**, so remote `http(s)` image URLs are skipped (logged as a warning).
- No `api_key` field is used for Bedrock.

### Prompt caching on Bedrock

Bedrock caching is driven by the same `cache_control` breakpoints as Anthropic —
Ferrox translates them into Converse `cachePoint` blocks, emitted immediately
after the content they terminate. The cache token counters Bedrock returns
(`cacheReadInputTokens` / `cacheWriteInputTokens`) are surfaced on both the
streaming and non-streaming paths, in the same shape as every other provider, so
they flow into metrics, logs and `usage_log` automatically.

Bedrock-specific constraints:

| Constraint | Behaviour |
|---|---|
| **Max 4 cache points per request** | If more are requested, Ferrox keeps the **last 4** and logs the rest at `debug`. Caching is prefix-based, so a later breakpoint covers everything an earlier one would. System breakpoints come first in request order and are therefore dropped first. |
| **Supported model families** | `cachePoint` is emitted only for Anthropic Claude and Amazon Nova models. Other families reject the block outright, so an unrecognised model deliberately gets no cache points rather than a failed request. |
| **TTL is fixed at 5 minutes** | Refreshed on each cache hit; not configurable. |
| **Minimum cacheable prefix varies by family** | Below the minimum a `cachePoint` is a **silent no-op**, not an error — the request succeeds with no caching. Watch `ferrox_tokens_total{type="cache_write"}` to confirm caching is actually happening. |

A request with no `cache_control` produces a Converse payload identical to
before caching support existed.

---

## Z.AI GLM

```yaml
providers:
  - name: glm-primary
    type: glm
    api_key: "${GLM_API_KEY}"
    base_url: "https://api.z.ai/api/paas/v4"
    # GLM Coding Plan: use https://api.z.ai/api/coding/paas/v4
```

GLM uses an OpenAI-compatible API, so Ferrox routes requests through the same adapter as OpenAI with no transformation. All OpenAI features (streaming, tool use, system prompts) work as-is.

The `base_url` already includes GLM's version prefix (`/v4`); the adapter appends `/chat/completions` to produce the correct endpoint `https://api.z.ai/api/paas/v4/chat/completions`.

Available models: `GLM-5.1`, `GLM-5`, `GLM-4.7`, `GLM-4.5-air`.

**Required env var:** `GLM_API_KEY`

---

## Prompt caching (`cache_control`)

Anthropic-style `cache_control` breakpoints are preserved across **every**
routing path, not just Anthropic-to-Anthropic:

| Path | `cache_control` |
|---|---|
| `/anthropic/v1/messages` → `type: anthropic` | Forwarded verbatim (whole body passes through untouched) |
| `/anthropic/v1/messages` → `type: openai` | Preserved |
| `/v1/chat/completions` → `type: anthropic` | Preserved |
| `/v1/chat/completions` → `type: openai` | Preserved |

Breakpoints are carried on individual content blocks and on messages. A
breakpoint on the **system** prompt is preserved too: the gateway's internal
representation holds the system prompt as a single string, so the breakpoint
travels alongside it and is re-attached as a system block when the request goes
to an Anthropic-format provider.

Requests that set no `cache_control` are forwarded byte-identically to before —
no empty keys are introduced.

> Anthropic-only content blocks that have no OpenAI equivalent (`thinking`,
> `document`, `search_result`) are still dropped when routing to an
> OpenAI-format provider, since the upstream would reject them. The dropped
> block's type is logged at `debug` level.

Cache effectiveness is measurable — see
[Observability](observability.md#prompt-cache-metrics) for the
`ferrox_tokens_total{type="cache_read"}` counter and a cache-hit-rate query.

---

## Using multiple entries of the same provider

Add multiple entries to multiply your rate limit budget or add geographic redundancy:

```yaml
providers:
  - name: anthropic-key-1
    type: anthropic
    api_key: "${ANTHROPIC_KEY_1}"

  - name: anthropic-key-2
    type: anthropic
    api_key: "${ANTHROPIC_KEY_2}"

models:
  - alias: claude-sonnet
    routing:
      strategy: round_robin
      targets:
        - provider: anthropic-key-1
          model_id: claude-sonnet-4-20250514
        - provider: anthropic-key-2
          model_id: claude-sonnet-4-20250514
```

This distributes requests evenly across two API keys, effectively doubling the rate limit.

---

## Per-provider overrides

Any provider can override the global `defaults` for timeouts, retries, and circuit breaker settings:

```yaml
providers:
  - name: anthropic-primary
    type: anthropic
    api_key: "${ANTHROPIC_API_KEY}"
    timeouts:
      ttfb_secs: 120    # extended for reasoning models
    retry:
      max_attempts: 2   # fail faster for primary; let fallback handle it
    circuit_breaker:
      failure_threshold: 3
```
