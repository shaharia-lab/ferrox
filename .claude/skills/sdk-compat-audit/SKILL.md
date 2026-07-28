---
name: sdk-compat-audit
description: >-
  Audit Ferrox's OpenAI ⇄ Anthropic translation layer against the official
  provider SDKs (freshly cloned into a temp dir), find where the proxy diverges
  from the real wire contracts, and file deduplicated GitHub issues for the gaps.
  Use to (re-)run the translation-fidelity / incompatibility check — e.g. after a
  provider ships API changes, before a release, or when a client reports a
  translation bug. Read-only against the codebase; the only writes are GitHub
  issues (and only after confirming they don't already exist).
---

# SDK compatibility audit

Ferrox normalises every provider to an internal **OpenAI-shaped** type
(`ChatCompletionRequest`/`Response`/`Chunk`) and re-exposes it in **both** an
OpenAI dialect (`/v1/chat/completions`) and an Anthropic dialect
(`/anthropic/v1/messages`). Every translation point is a place the proxy can
drift from what the official SDKs actually send and expect. This skill
cross-references those points against the SDKs' own accumulation / request /
response code — the authoritative contract — and turns real divergences into
GitHub issues.

The goal is **evidence-backed findings**, not vibes: every issue must cite a
concrete `ferrox/src/...:line` AND the SDK file that proves the correct
behaviour. If you can't cite both, drop the finding.

---

## Flow

```
0 preflight (gh + git + clean tree)
→ 1 clone the official SDKs into a temp dir (shallow)
→ 2 map Ferrox's translation surfaces (files/functions)
→ 3 audit each dimension: Ferrox code vs SDK contract (fan out read-only sub-agents)
→ 4 consolidate + self-check (kill un-cited findings)
→ 5 dedup against existing open issues, then file the survivors
→ 6 report
```

---

## Phase 0 — Preflight

- `gh auth status` (must be authenticated for `shaharia-lab`) and `jq` present → else STOP.
- Resolve `OWNER/REPO` from `gh repo view --json nameWithOwner` (normally `shaharia-lab/ferrox`).
- Work from a **synced** checkout: `git fetch origin --prune` then read code from
  `origin/<default-branch>` (or a clean local checkout). A stale checkout yields
  wrong line numbers and phantom findings.
- Pick a scratch dir: `SDKDIR="$(mktemp -d)/sdk-compat"` (or use the session
  scratchpad). Everything cloned goes here and is disposable.

## Phase 1 — Clone the official SDKs (shallow, into `$SDKDIR`)

Clone these read-only references. `--depth 1` is enough; skip any that fail
(note it) rather than aborting.

```bash
mkdir -p "$SDKDIR" && cd "$SDKDIR"
# OpenAI (request params + streaming accumulators)
git clone --depth 1 https://github.com/openai/openai-python
git clone --depth 1 https://github.com/openai/openai-node
git clone --depth 1 https://github.com/openai/openai-go
# Anthropic (message params + streaming accumulators)
git clone --depth 1 https://github.com/anthropics/anthropic-sdk-python
git clone --depth 1 https://github.com/anthropics/anthropic-sdk-typescript
git clone --depth 1 https://github.com/anthropics/anthropic-sdk-go
# Gemini (functionCall/functionResponse + content-part contracts) — best effort
git clone --depth 1 https://github.com/googleapis/python-genai
```

Highest-signal files (the authoritative contracts):

| Contract | Where in the SDKs |
|---|---|
| OpenAI streaming tool-call accumulation (merge by `index`, `id`/`name` first, `arguments` concatenated, clamp `-1`) | `openai-go/streamaccumulator.go`; `openai-python/src/openai/lib/streaming/_deltas.py`; `openai-node/src/lib/ChatCompletionStream.ts` |
| OpenAI request params (what a transparent proxy must forward) | `openai-python/src/openai/types/chat/completion_create_params.py` |
| OpenAI usage details (`prompt_tokens_details`/`completion_tokens_details`) | `openai-python .../types/completion_usage.py` |
| Anthropic streaming accumulation (one content block per `index`; `thinking`/`input_json_delta` concatenated; `input_tokens` from `message_start`, updated in `message_delta`) | `anthropic-sdk-python/src/anthropic/lib/streaming/_messages.py`; `anthropic-sdk-typescript/src/lib/*MessageStream.ts`; `anthropic-sdk-go/messageutil.go` |
| Anthropic content blocks / deltas (text, image sources base64 vs url, tool_use, tool_result incl. image, thinking/redacted_thinking, `thinking_delta`/`signature_delta`) | `anthropic-sdk-python/src/anthropic/types/*` |
| Anthropic stop reasons (`end_turn|max_tokens|stop_sequence|tool_use|pause_turn|refusal`) | `anthropic-sdk-python/src/anthropic/types/message.py` |
| Gemini functionCall/functionResponse + inline/file data | `python-genai/.../types.py` (search `FunctionCall`, `FunctionResponse`, `inline_data`, `file_data`) |

## Phase 2 — Map Ferrox's translation surfaces

The proxy's translation lives almost entirely in these files — read them (current
tree, not memory):

| Surface | File | Key items |
|---|---|---|
| Anthropic ⇄ internal (ingress + egress) | `ferrox/src/anthropic_types.rs` | `to_chat_completion_request`, `convert_blocks`, `to_anthropic_response`, `openai_stream_to_anthropic_sse`, `finish_reason_to_anthropic`, the SSE event constructors |
| Internal types | `ferrox/src/types.rs` | `ChatMessage`, `ChunkDelta`, `StreamToolCall`, `Usage`, `Choice`, response/chunk structs (+ any `#[serde(flatten)] extra`) |
| OpenAI/Kimi/GLM adapter | `ferrox/src/providers/openai.rs` | `OpenAIRequest` (forwarded fields + `extra`), streaming chunk parse |
| Native Anthropic adapter | `ferrox/src/providers/anthropic.rs` + `anthropic_events.rs` | request build, `AnthropicResponseContent`, `AnthropicEventProcessor`, `map_stop_reason`, incremental tool/thinking emission |
| Gemini adapter | `ferrox/src/providers/gemini.rs` | `functionCall`/`functionResponse`, `tool_config`, `finishReason` map, image inlining |
| Bedrock adapter | `ferrox/src/providers/bedrock.rs` | tools/`tool_choice`, tool_use blocks, content-block array |

## Phase 3 — Audit dimensions (Ferrox vs SDK contract)

For **each** dimension below, compare the Ferrox code to the SDK contract and
record any divergence. Fan out one **read-only** sub-agent per surface (or group)
so the audit runs in parallel — give each the exact scope, the Ferrox files, and
the `$SDKDIR` paths, and require every finding to cite `ferrox/src/...:line` +
the SDK file. Consolidate their structured findings.

Cover at least:

1. **Streaming tool calls** — are fragmented `tool_calls` deltas accumulated by
   `index`? Does a continuation fragment (no `id`/`type`/`name`) deserialize? Is a
   `-1` index clamped? One Anthropic `tool_use` block per call (not per fragment)?
2. **Reasoning / extended thinking** — is `reasoning_content` preserved on both
   dialects? Do Anthropic `thinking`/`redacted_thinking` blocks deserialize and
   map to `reasoning_content`? Is `thinking_delta`/`signature_delta` handled?
3. **Streaming usage** — is `input_tokens`/`prompt_tokens` non-zero on streaming
   (from `message_start` for Anthropic, final chunk for OpenAI)? Do
   `*_tokens_details` (cache/reasoning breakdowns) survive?
4. **Multimodal** — are `image` blocks (base64 **and** url) preserved (top-level
   and inside `tool_result`)? `data:` → base64 source (not `url`) for Anthropic?
   Bedrock content-block array? Gemini `inline_data`/`file_data` (+ remote fetch
   with an SSRF guard)?
5. **Tool calling per provider** — Gemini `functionCall`↔`tool_calls`,
   `functionResponse`, `tool_choice`→`functionCallingConfig`; Bedrock
   `tools`/`tool_choice` sent upstream, non-stream `tool_use` parsed, tool history.
6. **stop_reason / finish_reason** — every provider reason mapped to a **valid**
   target enum in both directions (`content_filter`→`refusal`, `pause_turn`,
   `model_context_window_exceeded`, `SAFETY`→`content_filter`, `tool_use`↔
   `tool_calls`); no raw pass-through of unknown values.
7. **Transparent field pass-through** — standard OpenAI request fields
   (`response_format`, `seed`, `n`, `logprobs`, penalties, `parallel_tool_calls`,
   `user`, `logit_bias`) forwarded; response `extra` (logprobs, `service_tier`)
   preserved; internal `_`-prefixed keys never leaked upstream.
8. **Errors & protocol** — mid-stream upstream errors emit an Anthropic `error`
   SSE event (not a bare close); no empty `text:""` block; system-block join with
   `\n`; unsupported/`document` blocks warn rather than drop silently.

Also **re-derive from the SDKs**, don't just check the list above — a provider may
have shipped a new field/reason/block type since this skill was written. The SDK
source is the source of truth.

## Phase 4 — Consolidate + self-check

Merge the sub-agents' findings, dedup across surfaces, and **kill any finding
that can't cite both** a `ferrox/src/...:line` and an SDK file proving the
expected behaviour. Prefer a short list of hard, evidence-backed gaps over a long
list of maybes. Note explicitly what you verified as **already correct** (so a
future run doesn't re-flag it).

## Phase 5 — Dedup, then file issues

Before filing anything, list existing issues so you don't duplicate:

```bash
gh issue list --repo OWNER/REPO --state all --limit 200 --json number,title,state
```

For each surviving finding, search that list for an equivalent (by area/title).
**If an open or recently-closed issue already covers it → do NOT file a new one;**
reference it in the report instead. Otherwise file it:

- One issue per distinct incompatibility, OR a small set grouped by theme
  (reasoning, usage, multimodal, tool-calling, transparency) plus a tracking
  **epic** that links them — mirror the structure the repo already uses
  (see the closed epic #76 and its children #70–#75 for the house style).
- Body **must** contain: the concrete `ferrox/src/...:line`, the SDK file that
  proves the correct behaviour, the user-visible consequence, and a one-line fix
  direction. Label `bug`.
- **Never invent a finding to have something to file.** A clean audit → file
  nothing and say so.

```bash
gh issue create --repo OWNER/REPO --label bug --title "<area>: <concise gap>" --body-file <path>
```

## Phase 6 — Report

Print a compact summary: SDKs cloned (+ any skipped), dimensions audited,
findings (issue # for each filed, or "already tracked in #N"), and what was
verified already-correct. Then remove `$SDKDIR`.

---

## Guardrails

- **Read-only on the codebase.** This skill never edits Ferrox source — it audits
  and files issues. (Implementation is a separate step, e.g. the
  `lab-workflow:github-issue-to-pr` flow.)
- **No unauthenticated network** beyond the public SDK clones. Don't clone
  anything not in the list without noting why.
- **Evidence or it didn't happen** — no finding without a Ferrox line + an SDK
  citation. When a check needs the code compiled/tested, prefer
  `. "$HOME/.cargo/env" && cargo test -p ferrox` (the toolchain is per-user
  rustup; `protoc`/`libssl` are not required to build).
- **Clean up** the temp SDK clones when done.
