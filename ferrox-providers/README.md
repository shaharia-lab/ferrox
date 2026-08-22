# ferrox-providers

The provider translation layer from [Ferrox](https://github.com/shaharia-lab/ferrox),
as a standalone library.

Everything needed to speak OpenAI's chat-completions shape to **Anthropic**,
**OpenAI**, **Google Gemini**, **AWS Bedrock** and **Z.AI GLM** — the request and
response types, the Anthropic Messages translation, and the adapters — with none
of the gateway around them. No routing, load balancing, circuit breaking, rate
limiting, JWKS or auth.

Ferrox itself consumes this crate, so the translation is exercised by Ferrox's
compat suite (169 tests live here) rather than maintained in two places.

## Usage

```toml
[dependencies]
ferrox-providers = { git = "https://github.com/shaharia-lab/ferrox", tag = "providers-v0.1.0",
                     default-features = false, features = ["anthropic", "openai"] }
```

Pin to a tag rather than a branch: a merge on Ferrox's `main` should never
silently change your build. The crate is a workspace member inside the Ferrox
repo — Cargo resolves that automatically, no separate repository needed.

```rust
use ferrox_providers::config::{DefaultsConfig, ProviderConfig, ProviderType};
use ferrox_providers::providers::build_registry;

let providers = vec![ProviderConfig {
    name: "anthropic".into(),
    provider_type: ProviderType::Anthropic,
    api_key: Some(std::env::var("ANTHROPIC_API_KEY")?),
    base_url: None,
    aws: None,
    timeouts: None,
    circuit_breaker: None,
}];

let registry = build_registry(&providers, &DefaultsConfig::default()).await?;
let adapter = &registry["anthropic"];
let response = adapter.chat(&request, "claude-sonnet-4-20250514").await?;
```

## Features

| Feature | Default | Effect |
| --- | :---: | --- |
| `anthropic` | ✅ | Anthropic Messages adapter |
| `openai` | ✅ | OpenAI adapter (also serves `ProviderType::Glm`) |
| `gemini` | ✅ | Google Gemini adapter |
| `bedrock` | — | AWS Bedrock adapter; pulls the AWS SDK |
| `axum` | — | `IntoResponse for ProxyError` and the Anthropic SSE event emitters |
| `openapi` | — | `utoipa::ToSchema` on the public response types |

The default build depends on **no web framework**: no `axum`, no `utoipa`, no
AWS SDK. That keeps a consumer off Ferrox's framework version treadmill and cuts
the dependency tree roughly in half (144 crates with `anthropic` alone, versus
266 with everything enabled).

Requesting a provider whose feature is disabled fails at `build_registry` time
with an actionable message, not at compile time — so a config file can name a
provider the binary was not built for and you get a clear error.

## MSRV

**1.88**, upheld for every feature combination *except* `bedrock`: the AWS SDK
crates declare `rust-version = 1.94.1` of their own, so enabling `bedrock` raises
the effective floor to theirs. This is the main reason `bedrock` is opt-in.

## Known rough edges

Carried over from the extraction and worth knowing before depending on the API:

- **`ProxyError` still carries gateway-flavoured variants** (`RateLimited`,
  `BudgetExceeded`, `CircuitOpen`) that this crate never constructs. Splitting
  the enum would ripple through every Ferrox handler, so it was left alone.
- **`DefaultsConfig` carries `retry` / `circuit_breaker`**, which the adapters
  never read. They are part of the same deserialized YAML object; dropping them
  would silently discard that gateway policy on round-trip. (`ProviderConfig`
  no longer carries a dead `retry` — removed in #145.)
- **`parse_sse_stream` takes a `reqwest::Response`**, so `reqwest`'s major
  version is part of the public API surface.

These would be worth resolving before any crates.io publish.

## License

MIT, same as Ferrox.
