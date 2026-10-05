//! Classifier-based routing: a classifier looks at a request and picks which
//! statically routed alias serves it.
//!
//! - [`Classifier`] is the backend-neutral interface. A backend answers one
//!   question — which of these tiers fits this conversation — and knows
//!   nothing about aliases, pools or fallbacks.
//! - [`ClassifierInput`] is what a backend sees: the text of the recent
//!   user and assistant turns, extracted the same way on every inbound
//!   surface and capped in size.
//! - [`RouteResolver`] is the single place the inbound handlers resolve a
//!   model alias. A statically routed alias costs the same map lookup as
//!   before; a classified one runs its classifier and falls back to the
//!   alias's `fallback_alias` on any failure, so the classifier can never
//!   fail a request.

mod input;
mod resolver;

#[cfg(test)]
pub(crate) mod test_support;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::config::{ClassifierConfig, ClassifierType};

pub use input::ClassifierInput;
pub use resolver::RouteResolver;

/// One option a classifier chooses between.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tier {
    /// The tier's name in the alias's `tiers` map; a classifier answers with
    /// one of these.
    pub name: String,
    /// Plain-language description of when the tier applies.
    #[allow(dead_code)] // read by classifier backends; none is built in yet
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
fn build_registry(configs: &[ClassifierConfig]) -> ClassifierRegistry {
    configs
        .iter()
        .map(|config| {
            let classifier: Arc<dyn Classifier> = match config.classifier_type {
                ClassifierType::Jev => {
                    tracing::warn!(
                        classifier = %config.id,
                        "Classifier backend 'jev' is not available in this build; \
                         aliases using it serve their fallback_alias"
                    );
                    Arc::new(UnavailableClassifier)
                }
            };
            (config.id.clone(), classifier)
        })
        .collect()
}

/// Stands in for a backend this build does not have. It never answers, so
/// every alias using it serves its `fallback_alias`.
struct UnavailableClassifier;

#[async_trait]
impl Classifier for UnavailableClassifier {
    async fn classify(
        &self,
        _input: &ClassifierInput,
        _tiers: &[Tier],
    ) -> Result<Classification, ClassifierError> {
        Err(ClassifierError::Failed(
            "classifier backend is not available in this build".to_string(),
        ))
    }
}
