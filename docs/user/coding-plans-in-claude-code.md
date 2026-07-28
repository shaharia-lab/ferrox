# Using GLM and Kimi Coding Plans in Claude Code

Z.AI (GLM) and Moonshot (Kimi) both sell **coding plan subscriptions** that are far cheaper than
per-token API pricing. Claude Code can talk to any Anthropic-compatible endpoint — but the coding
plans don't always expose one:

| Provider | Coding plan endpoint | Protocol | Works with Claude Code directly? |
|---|---|---|---|
| Z.AI (GLM) | `https://api.z.ai/api/anthropic` | Anthropic | Yes |
| Moonshot (Kimi) | `https://api.kimi.com/coding/v1` | **OpenAI only** | **No** |

Kimi is the problem case. Its coding plan is served **only** over an OpenAI-compatible API.
Moonshot does operate an Anthropic-compatible endpoint at `https://api.moonshot.ai/anthropic`, but
coding-plan keys are **not** valid against it — that endpoint bills the standard pay-as-you-go
platform account. So there is no way to point Claude Code at a Kimi coding plan on its own.

Ferrox closes that gap. It normalises every provider to one internal format and re-exposes it in
**both** dialects, so an Anthropic-speaking client like Claude Code can reach an OpenAI-only
backend:

```
Claude Code ──Anthropic──> Ferrox ──OpenAI────> Kimi coding plan
                             │
                             └────Anthropic────> Z.AI GLM coding plan
```

One gateway, one endpoint, one auth token in your Claude Code config — both subscriptions usable,
plus failover, rate limiting, and usage metrics across them.

---

## 1. Configure Ferrox

Create `config/local.yaml`. **Never commit API keys** — reference environment variables.

```yaml
providers:
  # Z.AI coding plan is served over the Anthropic-native endpoint.
  # Do NOT use https://api.z.ai/api/paas/v4 — that OpenAI-compatible route bills
  # the pay-as-you-go balance instead of your coding plan.
  - name: zai
    type: anthropic
    api_key: "${Z_AI_API_KEY}"
    base_url: "https://api.z.ai/api/anthropic"

  # Kimi coding plan is OpenAI-compatible only.
  # Do NOT use https://api.moonshot.ai/anthropic — coding-plan keys are rejected there.
  - name: moonshot
    type: openai
    api_key: "${MOONSHOT_API_KEY}"
    base_url: "https://api.kimi.com/coding/v1"

models:
  - alias: glm-5.2
    routing:
      strategy: failover
      targets:
        - provider: zai
          model_id: "glm-5.2"

  - alias: glm-4.5-air          # cheap/fast — good for the Haiku slot
    routing:
      strategy: failover
      targets:
        - provider: zai
          model_id: "glm-4.5-air"

  - alias: k3                   # Kimi K3, 1,048,576 token context
    routing:
      strategy: failover
      targets:
        - provider: moonshot
          model_id: "k3"

  - alias: kimi-coding          # K2.7 Coding, 262,144 token context
    routing:
      strategy: failover
      targets:
        - provider: moonshot
          model_id: "kimi-for-coding"

virtual_keys:
  - key: "${PROXY_KEY:-sk-local-dev}"
    name: claude-code
    allowed_models: ["*"]
    rate_limit:
      requests_per_minute: 120
      burst: 20
```

`virtual_keys` is what Claude Code authenticates with. Your real provider keys stay in Ferrox's
environment and never enter any Claude Code settings file.

### Finding current model IDs

Both providers expose an OpenAI-style model list. Query it rather than guessing:

```bash
curl -s https://api.kimi.com/coding/v1/models \
  -H "Authorization: Bearer $MOONSHOT_API_KEY" | jq '.data[].id'

curl -s https://api.z.ai/api/paas/v4/models \
  -H "Authorization: Bearer $Z_AI_API_KEY" | jq '.data[].id'
```

---

## 2. Run Ferrox

Gateway only — no control plane, database, or Redis needed for this setup.

```bash
docker run -d --name ferrox --restart unless-stopped \
  -p 127.0.0.1:2333:8080 \
  -e Z_AI_API_KEY -e MOONSHOT_API_KEY -e PROXY_KEY \
  -v "$(pwd)/config/local.yaml:/app/config/local.yaml:ro" \
  ghcr.io/shaharia-lab/ferrox:latest --config /app/config/local.yaml
```

Passing `-e VAR` without a value inherits it from your shell, so keys never appear in your shell
history or in `docker inspect` output as literals.

Pick a port that nothing else uses — `8080` is a common collision. Verify:

```bash
curl http://localhost:2333/healthz                     # {"status":"ok"}
curl http://localhost:2333/anthropic/v1/models \
  -H "Authorization: Bearer sk-local-dev"              # lists your aliases
```

---

## 3. Point Claude Code at Ferrox

Claude Code appends `/v1/messages` to `ANTHROPIC_BASE_URL`, and Ferrox serves its Anthropic surface
under `/anthropic` — so the base URL is `http://localhost:2333/anthropic`.

Keep a settings file per backend and select it with `--settings`.

**`~/.claude/settings_glm.json`**

```json
{
  "$schema": "https://json.schemastore.org/claude-code-settings.json",
  "env": {
    "ANTHROPIC_BASE_URL": "http://localhost:2333/anthropic",
    "ANTHROPIC_AUTH_TOKEN": "sk-local-dev",
    "API_TIMEOUT_MS": "3000000",
    "ANTHROPIC_DEFAULT_OPUS_MODEL": "glm-5.2",
    "ANTHROPIC_DEFAULT_SONNET_MODEL": "glm-5.2",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL": "glm-4.5-air"
  }
}
```

**`~/.claude/settings_kimi.json`**

```json
{
  "$schema": "https://json.schemastore.org/claude-code-settings.json",
  "env": {
    "ANTHROPIC_BASE_URL": "http://localhost:2333/anthropic",
    "ANTHROPIC_AUTH_TOKEN": "sk-local-dev",
    "API_TIMEOUT_MS": "3000000",
    "ANTHROPIC_MODEL": "k3",
    "ANTHROPIC_DEFAULT_OPUS_MODEL": "k3",
    "ANTHROPIC_DEFAULT_SONNET_MODEL": "k3",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL": "k3",
    "CLAUDE_CODE_SUBAGENT_MODEL": "k3",
    "CLAUDE_CODE_AUTO_COMPACT_WINDOW": "1048576"
  }
}
```

`ANTHROPIC_AUTH_TOKEN` is the Ferrox **virtual key**, not a provider key.

`CLAUDE_CODE_AUTO_COMPACT_WINDOW` is what actually tells Claude Code how much context it may use
before auto-compacting — set it to the model's real context window (K3 is 1,048,576).

### Run it

```bash
claude --settings ~/.claude/settings_glm.json  -p "Hi"
claude --settings ~/.claude/settings_kimi.json -p "Hi"
```

To make one the default for a shell session, export the same variables instead of using
`--settings`.

---

## Mixing both plans in one session

Because Ferrox routes per model alias, a single settings file can spread Claude Code's model slots
across **both** subscriptions — for example a large-context Kimi model for the main loop and a cheap
GLM model for the fast/background slot:

```json
"ANTHROPIC_DEFAULT_OPUS_MODEL": "k3",
"ANTHROPIC_DEFAULT_SONNET_MODEL": "k3",
"ANTHROPIC_DEFAULT_HAIKU_MODEL": "glm-4.5-air"
```

You can also make one plan fall back to the other when a provider is down, by giving an alias
multiple targets — see [Routing](routing.md):

```yaml
  - alias: primary
    routing:
      strategy: failover
      targets:
        - provider: moonshot
          model_id: "k3"
        - provider: zai
          model_id: "glm-5.2"
```

Because clients only ever see your aliases, you can repoint `primary` at a different vendor without
touching a single Claude Code settings file.

---

## Troubleshooting

**`Insufficient balance or no resource package` (Z.AI, HTTP 429, code 1113)**
You're on the pay-as-you-go route, not the coding plan. Ensure `base_url` is
`https://api.z.ai/api/anthropic` with `type: anthropic` — not `/api/paas/v4`.

**`Invalid Authentication` from Moonshot**
Coding-plan keys only work against `https://api.kimi.com/coding/v1`. They are rejected by
`api.moonshot.ai` and `api.moonshot.cn`, on both the OpenAI and Anthropic routes.

**`There's an issue with the selected model (…)` in Claude Code**
The model name never reached Ferrox — Claude Code rejected it locally. Use plain alias names.
Note that Claude Code strips its `[1m]` long-context suffix before sending, so `glm-5.2[1m]` arrives
at Ferrox as `glm-5.2`; use `CLAUDE_CODE_AUTO_COMPACT_WINDOW` to set the context window instead.

**`Model alias '…' is not configured` (HTTP 404)**
Ferrox matches aliases exactly. Check `GET /anthropic/v1/models` for the list it actually serves.

**Empty responses with `finish_reason: "length"`**
Kimi K3 is a reasoning-only model — a small `max_tokens` is consumed entirely by reasoning before
any text is emitted. Allow at least a few hundred tokens.

**404s on every route, or an unexpected HTML page**
Something else owns the port. Confirm with `ss -ltnp | grep <port>` and check that
`docker ps` shows a port mapping for the Ferrox container.

Ferrox logs every routed request with the resolved alias, provider, upstream model ID, and status —
`docker logs ferrox` is the fastest way to see whether a request arrived and where it went.

---

## See also

- [Providers](providers.md) — all supported provider types and their `base_url` conventions
- [Routing](routing.md) — failover, load balancing, circuit breakers
- [Virtual Keys](virtual-keys.md) — auth, rate limits, per-key model access
- [API Reference](api-reference.md) — full endpoint and payload reference
