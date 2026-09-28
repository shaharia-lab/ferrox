# Using Codex CLI with Ferrox

[Codex CLI](https://github.com/openai/codex) talks the OpenAI **Responses API**
(`POST /v1/responses`). Ferrox serves that endpoint for **every** configured provider, so Codex can
run on Anthropic, Gemini, Bedrock, or a GLM / Kimi coding plan, with Ferrox's failover, rate
limiting, budgets and usage metrics in front of it:

```
Codex CLI ──Responses──> Ferrox ──Anthropic──> Z.AI GLM coding plan
                           │
                           └──────OpenAI─────> Kimi coding plan / any OpenAI-compatible API
```

See the [API reference](api-reference.md#post-v1responses) for exactly what the endpoint supports.

---

## 1. Configure and run Ferrox

Any Ferrox config works. The GLM + Kimi coding-plan config from
[Using GLM and Kimi Coding Plans in Claude Code](coding-plans-in-claude-code.md#1-configure-ferrox)
is a good starting point: Codex uses the same aliases (`glm-5.2`, `k3`, …) and the same virtual key.
Start Ferrox as described there, and check that it answers:

```bash
curl http://localhost:2333/v1/models -H "Authorization: Bearer sk-local-dev"
```

---

## 2. Point Codex at Ferrox

Add a model provider to `~/.codex/config.toml`:

```toml
model = "glm-5.2"          # any Ferrox alias
model_provider = "ferrox"

# Ferrox cannot run OpenAI-hosted tools. Codex sends these two by default and
# Ferrox rejects them with a 400, so switch them off:
web_search = "disabled"    # the hosted `web_search` tool

[features]
multi_agent = false        # sub-agent tools, sent as a `namespace` tool

[model_providers.ferrox]
name = "Ferrox"
base_url = "http://localhost:2333/v1"
env_key = "FERROX_API_KEY"
wire_api = "responses"
```

Keep the top-level keys (`web_search` included) **above** the first `[table]` header. TOML assigns
any key written after a header to that table, and Codex then ignores it with an "unrecognized
configuration setting" warning.

Then export the Ferrox virtual key (not a provider key) and run Codex:

```bash
export FERROX_API_KEY=sk-local-dev
codex                                   # interactive
codex exec "fix the failing test"       # one-shot
codex -m k3                             # another alias for this session
```

Codex prints `Model metadata for 'glm-5.2' not found. Defaulting to fallback metadata`. That is
expected: Codex only ships metadata for OpenAI's own models, so it uses defaults for any other
name.

---

## How it behaves

- **Stateless.** Codex sends the full conversation on every turn (`store: false`), which is what
  Ferrox needs. `codex exec resume` and multi-turn sessions work the same way.
- **Tools.** Codex's own tools (`exec_command`, `write_stdin`, `view_image`, …) are plain
  `function` tools and work on every provider. File edits go through `apply_patch` inside
  `exec_command`.
- **Reasoning.** Reasoning from providers that return it (GLM, Kimi, Claude) comes back as
  `reasoning` output items. On Anthropic, Codex's `model_reasoning_effort` turns on extended
  thinking for the target model: adaptive thinking with that effort on Claude 4.6+, a thinking
  budget on Claude 3.7–4.5. GLM and Kimi behind the Anthropic adapter are left as they are. The thinking block's signature comes back as the item's
  `encrypted_content`, so the next turn replays it and tool loops keep working.

## Troubleshooting

| Symptom | Cause / fix |
|---|---|
| `built-in tool 'web_search' is not supported` | Add `web_search = "disabled"` above the first `[table]` header |
| `built-in tool 'namespace' is not supported` | Add `multi_agent = false` under `[features]` |
| `401 unauthorized` | `FERROX_API_KEY` is unset or isn't a Ferrox virtual key |
| `404 model_not_found` | `model` isn't one of the aliases from `GET /v1/models` |
| `previous_response_id is not supported` | A client configured for server-side conversation state. Ferrox is stateless, so the client must send the full `input` |
