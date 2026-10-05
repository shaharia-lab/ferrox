//! The Jev backend: TypeSafe's System One model, asked one `choice` question
//! per request.
//!
//! One `POST {base_url}/v1/systemone` carries the capped turns as `state` and
//! a single question whose options are the alias's tiers, each described by
//! its `when` text. The answer's `choice` is the tier.
//!
//! An error response's body is never read: Jev's 422 body echoes the request
//! `state`, so only the HTTP status and the `x-typesafe-request-id` header
//! are logged.

use std::collections::HashMap;

use anyhow::Context as _;
use async_trait::async_trait;
use reqwest::header::{HeaderValue, AUTHORIZATION};
use reqwest::StatusCode;
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};

use super::{Classification, Classifier, ClassifierError, ClassifierInput, Tier};
use crate::config::ClassifierConfig;

/// Id of the one question in a request. Jev never shows it to the model.
const QUESTION_ID: &str = "tier";

/// What the model is asked; the tiers' `when` texts are the options.
const INSTRUCTIONS: &str =
    "Which option best describes the latest user request in this conversation?";

/// Identifies a call to TypeSafe support.
const REQUEST_ID_HEADER: &str = "x-typesafe-request-id";

pub struct JevClassifier {
    /// The id of the `classifiers` entry, for logs.
    id: String,
    client: reqwest::Client,
    url: reqwest::Url,
    /// `Bearer <api_key>`, marked sensitive so it is never printed.
    authorization: HeaderValue,
    model: String,
}

impl JevClassifier {
    pub fn new(config: &ClassifierConfig, client: reqwest::Client) -> Result<Self, anyhow::Error> {
        let mut authorization = HeaderValue::from_str(&format!("Bearer {}", config.api_key))
            .ok()
            .with_context(|| {
                format!(
                    "Classifier '{}': api_key is not a valid HTTP header value",
                    config.id
                )
            })?;
        authorization.set_sensitive(true);
        let url = format!("{}/v1/systemone", config.base_url.trim_end_matches('/'));
        let url = reqwest::Url::parse(&url)
            .ok()
            .filter(|url| matches!(url.scheme(), "http" | "https"))
            .with_context(|| {
                format!(
                    "Classifier '{}': base_url is not a valid http(s) URL",
                    config.id
                )
            })?;
        Ok(Self {
            id: config.id.clone(),
            client,
            url,
            authorization,
            model: config.model.clone(),
        })
    }

    /// The failure for a request that got no complete response.
    fn unreachable(&self, error: &reqwest::Error, message: &'static str) -> ClassifierError {
        // The request could not even be built: no later call will fare
        // better, so this is not an outage.
        if error.is_builder() {
            tracing::error!(
                classifier = %self.id,
                "Classifier request could not be built; check its configuration"
            );
            return ClassifierError::Failed("classifier request could not be built".to_string());
        }
        let kind = if error.is_connect() {
            "connect"
        } else if error.is_timeout() {
            "timeout"
        } else if error.is_body() || error.is_decode() {
            "body"
        } else {
            "request"
        };
        tracing::warn!(classifier = %self.id, kind, "{message}");
        ClassifierError::Unavailable(format!("classifier request failed ({kind})"))
    }

    /// The failure for a response that is not a 2xx. Its body is left unread.
    fn rejected(&self, response: &reqwest::Response) -> ClassifierError {
        let status = response.status();
        let request_id = response
            .headers()
            .get(REQUEST_ID_HEADER)
            .and_then(|v| v.to_str().ok());
        let message = format!("classifier returned HTTP {}", status.as_u16());
        let unavailable = status == StatusCode::TOO_MANY_REQUESTS
            || status == StatusCode::REQUEST_TIMEOUT
            || status.is_server_error();
        if unavailable {
            tracing::warn!(
                classifier = %self.id,
                status = status.as_u16(),
                typesafe_request_id = request_id,
                "Classifier is unavailable"
            );
            ClassifierError::Unavailable(message)
        } else {
            // 401 and 422 above all: a wrong key or a request Jev will never
            // accept. Waiting does not fix either.
            tracing::error!(
                classifier = %self.id,
                status = status.as_u16(),
                typesafe_request_id = request_id,
                "Classifier rejected the request; check its configuration"
            );
            ClassifierError::Failed(message)
        }
    }
}

#[async_trait]
impl Classifier for JevClassifier {
    async fn classify(
        &self,
        input: &ClassifierInput,
        tiers: &[Tier],
    ) -> Result<Classification, ClassifierError> {
        let request = Request {
            state: State(input),
            model: &self.model,
            questions: Questions(Question {
                question_type: "choice",
                instructions: INSTRUCTIONS,
                criteria: Criteria(tiers),
            }),
        };
        let response = self
            .client
            .post(self.url.clone())
            .header(AUTHORIZATION, self.authorization.clone())
            .json(&request)
            .send()
            .await
            .map_err(|e| self.unreachable(&e, "Classifier request failed"))?;
        if !response.status().is_success() {
            return Err(self.rejected(&response));
        }
        let body = response
            .bytes()
            .await
            .map_err(|e| self.unreachable(&e, "Classifier response was cut off"))?;

        // A parse error is reported by position only: its message can quote
        // the body.
        let mut body: Response = serde_json::from_slice(&body).map_err(|e| {
            tracing::warn!(
                classifier = %self.id,
                line = e.line(),
                column = e.column(),
                "Classifier response is not a System One answer"
            );
            ClassifierError::Failed("classifier response is malformed".to_string())
        })?;
        let Some(answer) = body.answers.remove(QUESTION_ID) else {
            tracing::warn!(classifier = %self.id, "Classifier response has no answer");
            return Err(ClassifierError::Failed(
                "classifier response is malformed".to_string(),
            ));
        };
        if !tiers.iter().any(|t| t.name == answer.choice) {
            return Err(ClassifierError::UnknownChoice);
        }
        // Jev returns the probabilities in no particular order; report them
        // in the tiers'.
        let probabilities = tiers
            .iter()
            .filter_map(|t| Some((t.name.clone(), *answer.probabilities.get(&t.name)?)))
            .collect();
        Ok(Classification {
            tier: answer.choice,
            confidence: answer.confidence,
            probabilities,
            model: body.model,
            input_tokens: body.usage.input_tokens,
        })
    }
}

// ── Wire types ───────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct Request<'a> {
    state: State<'a>,
    model: &'a str,
    questions: Questions<'a>,
}

/// The turns, oldest first, as `[{"role": "user", "text": "…"}, …]`.
struct State<'a>(&'a ClassifierInput);

impl Serialize for State<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Turn<'a> {
            role: &'static str,
            text: &'a str,
        }
        serializer.collect_seq(self.0.turns.iter().map(|turn| Turn {
            role: turn.role.as_str(),
            text: &turn.text,
        }))
    }
}

/// The one question, under [`QUESTION_ID`].
struct Questions<'a>(Question<'a>);

impl Serialize for Questions<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_map([(QUESTION_ID, &self.0)])
    }
}

#[derive(Serialize)]
struct Question<'a> {
    #[serde(rename = "type")]
    question_type: &'static str,
    instructions: &'static str,
    criteria: Criteria<'a>,
}

/// Tier name → its `when` text.
struct Criteria<'a>(&'a [Tier]);

impl Serialize for Criteria<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_map(self.0.iter().map(|tier| (&tier.name, &tier.when)))
    }
}

/// Only `answers.<id>.choice` is required: a response that names a tier is
/// usable even if Jev stops reporting something else.
#[derive(Deserialize)]
struct Response {
    #[serde(default)]
    model: String,
    answers: HashMap<String, Answer>,
    #[serde(default)]
    usage: Usage,
}

#[derive(Deserialize)]
struct Answer {
    choice: String,
    #[serde(default)]
    confidence: Option<f64>,
    #[serde(default)]
    probabilities: HashMap<String, f64>,
}

#[derive(Deserialize, Default)]
struct Usage {
    #[serde(default)]
    input_tokens: u32,
}
