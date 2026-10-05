//! Classifier-based routing: a classifier looks at a request and picks which
//! statically routed alias serves it.
//!
//! - [`Classifier`] is the backend-neutral interface. A backend answers one
//!   question — which of these tiers fits this conversation — and knows
//!   nothing about aliases, pools or fallbacks.
//! - [`ClassifierInput`] is what a backend sees: the text of the recent
//!   user and assistant turns, extracted the same way on every inbound
//!   surface and capped in size.
//! - `jev` is the one backend so far: TypeSafe's System One model.
//! - [`RouteResolver`] is the single place the inbound handlers resolve a
//!   model alias. A statically routed alias costs the same map lookup as
//!   before; a classified one runs its classifier and falls back to the
//!   alias's `fallback_alias` on any failure, so the classifier can never
//!   fail a request. An alias in shadow mode, and a request that opts out
//!   with the `x-ferrox-classifier: skip` header, are served by the
//!   `fallback_alias` too.
//! - `cache` keeps each classifier's recent answers in memory, so an input
//!   it has already answered is routed without calling it again.

mod cache;
mod input;
mod jev;
mod resolver;

#[cfg(test)]
mod jev_tests;
#[cfg(test)]
pub(crate) mod test_support;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::config::{ClassifierConfig, ClassifierType};

pub use input::ClassifierInput;
pub use resolver::{skip_requested, RouteResolver};

/// One option a classifier chooses between.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tier {
    /// The tier's name in the alias's `tiers` map; a classifier answers with
    /// one of these.
    pub name: String,
    /// Plain-language description of when the tier applies.
    pub when: String,
}

/// A classifier's answer.
#[derive(Debug, Clone, PartialEq)]
pub struct Classification {
    /// Name of the chosen tier.
    pub tier: String,
    /// How sure the classifier is, 0 to 1. `None` when the backend has no
    /// calibrated number to give (a chat model asked to pick a tier); such
    /// an answer is never treated as low confidence.
    pub confidence: Option<f64>,
    /// Probability per tier name, when the backend reports them.
    pub probabilities: Vec<(String, f64)>,
    /// The classifier model that answered, as the backend reports it.
    pub model: String,
    /// Tokens the classifier billed for this input.
    pub input_tokens: u32,
}

/// Why a classifier gave no answer. The message never carries request text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClassifierError {
    #[error("classifier timed out")]
    Timeout,
    #[error("request has no user text to classify")]
    NoInput,
    /// The classifier's circuit breaker is open; it was not called.
    #[error("classifier circuit breaker is open")]
    BreakerOpen,
    /// The classifier answered with an option it was not offered.
    #[error("classifier chose an option that was not offered")]
    UnknownChoice,
    /// The classifier could not be reached or is overloaded (a connection
    /// error, HTTP 408, 429 or 5xx). Counts against its circuit breaker, as a
    /// timeout does.
    #[error("{0}")]
    Unavailable(String),
    /// Any other failure: one that says nothing about whether the next call
    /// would succeed, or that waiting cannot fix (a rejected key or request).
    #[error("{0}")]
    Failed(String),
}

/// A classifier backend. One call is one attempt: retries, the time limit,
/// the confidence threshold and the fallback all belong to [`RouteResolver`].
#[async_trait]
pub trait Classifier: Send + Sync {
    /// Choose one of `tiers` for `input`. `input` has at least one turn and
    /// ends with a user turn.
    async fn classify(
        &self,
        input: &ClassifierInput,
        tiers: &[Tier],
    ) -> Result<Classification, ClassifierError>;
}

/// Classifier id (`classifiers[].id`) → backend.
pub type ClassifierRegistry = HashMap<String, Arc<dyn Classifier>>;

/// Build the backend of every configured classifier.
fn build_registry(configs: &[ClassifierConfig]) -> Result<ClassifierRegistry, anyhow::Error> {
    // One connection pool for every classifier.
    let client = reqwest::Client::new();
    configs
        .iter()
        .map(|config| {
            let classifier: Arc<dyn Classifier> = match config.classifier_type {
                ClassifierType::Jev => Arc::new(jev::JevClassifier::new(config, client.clone())?),
            };
            Ok((config.id.clone(), classifier))
        })
        .collect()
}
