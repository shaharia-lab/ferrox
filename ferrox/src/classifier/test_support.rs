//! A fake classifier and a small classified gateway, for the resolver's
//! tests and the handlers'.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{json, Value};

use super::{
    Classification, Classifier, ClassifierError, ClassifierInput, ClassifierRegistry, Tier,
};
use crate::config::Config;
use crate::error::ProxyError;
use crate::providers::{ProviderAdapter, ProviderRegistry, ProviderStream};
use crate::types::{ChatCompletionRequest, ChatCompletionResponse};

/// What a [`FakeClassifier`] does with every call.
#[derive(Debug, Clone)]
pub(crate) enum Answer {
    /// Choose this tier, with this confidence.
    Tier(&'static str, Option<f64>),
    Fail(ClassifierError),
    /// Never answer.
    Hang,
}

/// The [`Classification`] a [`FakeClassifier`] gives for [`Answer::Tier`].
pub(crate) fn answer(tier: &str, confidence: Option<f64>) -> Classification {
    Classification {
        tier: tier.to_string(),
        confidence,
        probabilities: vec![(tier.to_string(), confidence.unwrap_or(1.0))],
        model: "fake-1".to_string(),
        input_tokens: 3,
    }
}

/// A classifier that always gives the same [`Answer`], counts its calls and
/// remembers the last input and tier names it was given.
pub(crate) struct FakeClassifier {
    reply: Answer,
    calls: AtomicUsize,
    seen: Mutex<Option<(ClassifierInput, Vec<String>)>>,
}

impl FakeClassifier {
    pub(crate) fn new(reply: Answer) -> Arc<Self> {
        Arc::new(Self {
            reply,
            calls: AtomicUsize::new(0),
            seen: Mutex::new(None),
        })
    }

    pub(crate) fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    pub(crate) fn seen(&self) -> Option<(ClassifierInput, Vec<String>)> {
        self.seen.lock().unwrap().clone()
    }
}

#[async_trait]
impl Classifier for FakeClassifier {
    async fn classify(
        &self,
        input: &ClassifierInput,
        tiers: &[Tier],
    ) -> Result<Classification, ClassifierError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let names = tiers.iter().map(|t| t.name.clone()).collect();
        *self.seen.lock().unwrap() = Some((input.clone(), names));
        match &self.reply {
            Answer::Tier(tier, confidence) => Ok(answer(tier, *confidence)),
            Answer::Fail(e) => Err(e.clone()),
            Answer::Hang => std::future::pending().await,
        }
    }
}

/// A registry whose classifier `fake` (the one [`gateway_config`] uses) is
/// `classifier`.
pub(crate) fn registry_with(classifier: Arc<FakeClassifier>) -> ClassifierRegistry {
    let classifier: Arc<dyn Classifier> = classifier;
    ClassifierRegistry::from([("fake".to_string(), classifier)])
}

/// A gateway with two statically routed aliases — `fast` on provider
/// `fast-up`, `smart` on `smart-up` — and `auto`, classified by `fake`
/// between `simple` → `fast` and `complex` → `smart`, falling back to `smart`
/// below a confidence of 0.6. `classifier` overrides fields of the `fake`
/// classifier's config.
///
/// Virtual keys: `sk-all` may use every alias, `sk-auto` only `auto`,
/// `sk-fast` only `fast`.
pub(crate) fn gateway_config(classifier: Value) -> Config {
    let mut fake = json!({"id": "fake", "type": "jev", "api_key": "k", "timeout_ms": 50});
    for (field, value) in classifier.as_object().into_iter().flatten() {
        fake[field] = value.clone();
    }
    let alias = |alias: &str, provider: &str| {
        json!({"alias": alias, "routing": {"strategy": "failover",
               "targets": [{"provider": provider, "model_id": format!("{alias}-v1")}]}})
    };
    let key =
        |key: &str, allowed: &[&str]| json!({"key": key, "name": key, "allowed_models": allowed});

    let config: Config = serde_json::from_value(json!({
        "defaults": {"retry": {"max_attempts": 1, "initial_backoff_ms": 1,
                               "max_backoff_ms": 1, "jitter": false}},
        "providers": [{"name": "fast-up", "type": "openai"}, {"name": "smart-up", "type": "openai"}],
        "classifiers": [fake],
        "models": [
            alias("fast", "fast-up"),
            alias("smart", "smart-up"),
            {"alias": "auto", "classifier": {
                "use": "fake",
                "confidence_threshold": 0.6,
                "fallback_alias": "smart",
                "tiers": {"simple": {"alias": "fast", "when": "Short factual or chat"},
                          "complex": {"alias": "smart", "when": "Multi-step reasoning or code"}}}},
        ],
        "virtual_keys": [key("sk-all", &["*"]), key("sk-auto", &["auto"]), key("sk-fast", &["fast"])],
    }))
    .unwrap();
    crate::config::validate(&config).unwrap();
    config
}

/// `config` with its classified alias `auto` in shadow mode.
pub(crate) fn shadowed(mut config: Config) -> Config {
    for model in &mut config.models {
        if let Some(classified) = &mut model.classifier {
            classified.shadow = true;
        }
    }
    config
}

/// A provider that answers every request with "Hello" (11 prompt / 7
/// completion tokens), reporting `<name>-model` as the upstream model.
pub(crate) struct Upstream {
    name: &'static str,
}

impl Upstream {
    /// The providers [`gateway_config`] routes to.
    pub(crate) fn registry() -> ProviderRegistry {
        ["fast-up", "smart-up"]
            .into_iter()
            .map(|name| {
                let provider: Arc<dyn ProviderAdapter> = Arc::new(Upstream { name });
                (name.to_string(), provider)
            })
            .collect()
    }

    fn model(&self) -> String {
        format!("{}-model", self.name)
    }
}

#[async_trait]
impl ProviderAdapter for Upstream {
    fn name(&self) -> &str {
        self.name
    }

    async fn chat(
        &self,
        _req: &ChatCompletionRequest,
        _model_id: &str,
    ) -> Result<ChatCompletionResponse, ProxyError> {
        Ok(serde_json::from_value(json!({
            "id": "chatcmpl-1", "object": "chat.completion", "created": 1, "model": self.model(),
            "choices": [{"index": 0, "finish_reason": "stop",
                         "message": {"role": "assistant", "content": "Hello"}}],
            "usage": {"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18}
        }))
        .unwrap())
    }

    async fn chat_stream(
        &self,
        _req: &ChatCompletionRequest,
        _model_id: &str,
    ) -> Result<ProviderStream, ProxyError> {
        let chunk = |choices: Value, usage: Value| {
            Ok(serde_json::from_value(json!({
                "id": "chatcmpl-1", "object": "chat.completion.chunk", "created": 1,
                "model": self.model(), "choices": choices, "usage": usage
            }))
            .unwrap())
        };
        let chunks = vec![
            chunk(
                json!([{"index": 0, "delta": {"role": "assistant", "content": "Hello"},
                        "finish_reason": "stop"}]),
                Value::Null,
            ),
            chunk(
                json!([]),
                json!({"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18}),
            ),
        ];
        Ok(Box::pin(futures::stream::iter(chunks)))
    }
}
