//! Ferrox's provider translation layer, as a standalone library.
//!
//! Everything needed to speak OpenAI's chat-completions shape to Anthropic,
//! OpenAI, Google Gemini and AWS Bedrock — the request/response types, the
//! Anthropic Messages translation, and the adapters themselves — with none of
//! the gateway around them (no routing, load balancing, circuit breaking, rate
//! limiting or auth).
//!
//! # Features
//!
//! | Feature | Default | Effect |
//! |---|---|---|
//! | `anthropic`, `openai`, `gemini` | ✅ | adapter for that provider |
//! | `bedrock` | — | AWS Bedrock adapter; pulls the AWS SDK |
//! | `axum` | — | `IntoResponse for ProxyError` + the Anthropic SSE emitters |
//! | `openapi` | — | `utoipa::ToSchema` on the public response types |
//!
//! The default build depends on no web framework, so embedding this crate does
//! not pin your application to Ferrox's `axum` or `utoipa` versions.
//!
//! ```toml
//! ferrox-providers = { git = "https://github.com/shaharia-lab/ferrox", tag = "providers-v0.1.0",
//!                      default-features = false, features = ["anthropic", "openai"] }
//! ```

pub mod anthropic_types;
pub mod config;
pub mod error;
pub mod providers;
pub mod types;
