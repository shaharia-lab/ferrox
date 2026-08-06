//! AWS Bedrock provider.
//!
//! Talks to Bedrock through the **Converse / ConverseStream** API — a single,
//! model-agnostic wire contract that covers every Bedrock family (Anthropic
//! Claude, Amazon Nova/Titan, Meta Llama, Mistral, Cohere, …). Requests arrive
//! in Ferrox's OpenAI-compatible shape and are translated to Converse here;
//! responses (and streaming events) are translated back.
//!
//! Credentials are resolved from the provider's `aws` config block: static
//! keys, a named profile/SSO, or an STS AssumeRole layered on top of either —
//! falling back to the standard AWS default credential chain when no explicit
//! source is configured. See [`resolve_sdk_config`].

use async_trait::async_trait;
use aws_config::Region;
use aws_sdk_bedrockruntime::types::{
    AnyToolChoice, AutoToolChoice, ContentBlock, ContentBlockDelta, ContentBlockStart,
    ConversationRole, ConverseOutput, ConverseStreamOutput, ImageBlock, ImageFormat, ImageSource,
    InferenceConfiguration, Message, ReasoningContentBlockDelta, SpecificToolChoice, StopReason,
    SystemContentBlock, TokenUsage, Tool, ToolChoice, ToolConfiguration, ToolInputSchema,
    ToolResultBlock, ToolResultContentBlock, ToolSpecification, ToolUseBlock,
};
use aws_smithy_types::{Blob, Document, Number};
use serde_json::Value;
use uuid::Uuid;

use crate::config::{AwsConfig, DefaultsConfig, ProviderConfig};
use crate::error::ProxyError;
use crate::providers::anthropic_events::{
    make_final_chunk, make_reasoning_chunk, make_text_chunk, make_tool_call_args_chunk,
    make_tool_call_start_chunk,
};
use crate::providers::{ProviderAdapter, ProviderStream};
use crate::types::{
    ChatCompletionRequest, ChatCompletionResponse, ChatMessage, Choice, ContentPart, FunctionCall,
    MessageContent, StopSequences, ToolCall, Usage,
};

// ── Adapter ──────────────────────────────────────────────────────────────────

pub struct BedrockAdapter {
    name: String,
    client: aws_sdk_bedrockruntime::Client,
}

impl BedrockAdapter {
    pub async fn new(
        cfg: &ProviderConfig,
        _defaults: &DefaultsConfig,
    ) -> Result<Self, anyhow::Error> {
        let sdk_config = resolve_sdk_config(cfg.aws.as_ref()).await;
        let client = aws_sdk_bedrockruntime::Client::new(&sdk_config);
        Ok(Self {
            name: cfg.name.clone(),
            client,
        })
    }
}

/// Resolve an [`aws_config::SdkConfig`] from the provider's `aws` block.
///
/// Base credential source (mutually exclusive, validated at config load):
///   * static `access_key_id` + `secret_access_key` (+ optional `session_token`)
///   * a named `profile`
///   * neither → the AWS default credential chain (env, `~/.aws`, SSO, instance
///     roles / IRSA)
///
/// When `assume_role` is set, an STS AssumeRole provider is layered on top of
/// the resolved base source; the SDK refreshes the temporary credentials.
async fn resolve_sdk_config(aws: Option<&AwsConfig>) -> aws_config::SdkConfig {
    let region = aws.and_then(|a| a.region.clone());
    let endpoint = aws.and_then(|a| a.endpoint_url.clone());
    let auth = aws.and_then(|a| a.auth.as_ref());

    // Build the base loader: region, endpoint override, and base credentials.
    let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
    if let Some(r) = &region {
        loader = loader.region(Region::new(r.clone()));
    }
    if let Some(e) = &endpoint {
        loader = loader.endpoint_url(e.clone());
    }
    if let Some(auth) = auth {
        match (&auth.access_key_id, &auth.secret_access_key) {
            (Some(ak), Some(sk)) => {
                let creds = aws_credential_types::Credentials::new(
                    ak.clone(),
                    sk.clone(),
                    auth.session_token.clone(),
                    None,
                    "ferrox-static",
                );
                loader = loader.credentials_provider(creds);
            }
            _ => {
                if let Some(profile) = &auth.profile {
                    loader = loader.profile_name(profile.clone());
                }
            }
        }
    }
    let base = loader.load().await;

    // Optionally assume a role on top of the base credentials.
    if let Some(role) = auth.and_then(|a| a.assume_role.as_ref()) {
        let mut builder = aws_config::sts::AssumeRoleProvider::builder(role.role_arn.clone())
            .session_name(
                role.session_name
                    .clone()
                    .unwrap_or_else(|| "ferrox".to_string()),
            );
        if let Some(eid) = &role.external_id {
            builder = builder.external_id(eid.clone());
        }
        if let Some(secs) = role.duration_secs {
            builder = builder.session_length(std::time::Duration::from_secs(secs));
        }
        if let Some(r) = &region {
            builder = builder.region(Region::new(r.clone()));
        }
        let provider = builder.configure(&base).build().await;

        let mut fin = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .credentials_provider(provider);
        if let Some(r) = &region {
            fin = fin.region(Region::new(r.clone()));
        }
        if let Some(e) = &endpoint {
            fin = fin.endpoint_url(e.clone());
        }
        return fin.load().await;
    }

    base
}

#[async_trait]
impl ProviderAdapter for BedrockAdapter {
    fn name(&self) -> &str {
        &self.name
    }

    async fn chat(
        &self,
        req: &ChatCompletionRequest,
        model_id: &str,
    ) -> Result<ChatCompletionResponse, ProxyError> {
        let built = build_converse_request(req, model_id)?;

        let mut call = self
            .client
            .converse()
            .model_id(model_id)
            .set_messages(Some(built.messages));
        for s in built.system {
            call = call.system(s);
        }
        if let Some(ic) = built.inference {
            call = call.inference_config(ic);
        }
        if let Some(tc) = built.tools {
            call = call.tool_config(tc);
        }

        let out = call.send().await.map_err(aws_err)?;

        Ok(converse_output_to_openai(
            out.output(),
            out.stop_reason(),
            out.usage(),
            model_id,
        ))
    }

    async fn chat_stream(
        &self,
        req: &ChatCompletionRequest,
        model_id: &str,
    ) -> Result<ProviderStream, ProxyError> {
        let built = build_converse_request(req, model_id)?;

        let mut call = self
            .client
            .converse_stream()
            .model_id(model_id)
            .set_messages(Some(built.messages));
        for s in built.system {
            call = call.system(s);
        }
        if let Some(ic) = built.inference {
            call = call.inference_config(ic);
        }
        if let Some(tc) = built.tools {
            call = call.tool_config(tc);
        }

        let mut stream = call.send().await.map_err(aws_err)?.stream;

        let model_id = model_id.to_string();
        let message_id = format!("chatcmpl-{}", Uuid::new_v4());

        let chunk_stream = async_stream::stream! {
            let mut stop_reason: Option<String> = None;
            let mut usage: Option<Usage> = None;

            loop {
                match stream.recv().await {
                    Ok(Some(event)) => match event {
                        ConverseStreamOutput::ContentBlockStart(e) => {
                            let index = e.content_block_index().max(0) as u32;
                            if let Some(ContentBlockStart::ToolUse(tu)) = e.start() {
                                yield Ok(make_tool_call_start_chunk(
                                    &message_id,
                                    &model_id,
                                    index,
                                    tu.tool_use_id(),
                                    tu.name(),
                                ));
                            }
                        }
                        ConverseStreamOutput::ContentBlockDelta(e) => {
                            let index = e.content_block_index().max(0) as u32;
                            match e.delta() {
                                Some(ContentBlockDelta::Text(t)) if !t.is_empty() => {
                                    yield Ok(make_text_chunk(&message_id, &model_id, t.clone()));
                                }
                                Some(ContentBlockDelta::ToolUse(tu)) if !tu.input().is_empty() => {
                                    yield Ok(make_tool_call_args_chunk(
                                        &message_id,
                                        &model_id,
                                        index,
                                        tu.input(),
                                    ));
                                }
                                Some(ContentBlockDelta::ReasoningContent(
                                    ReasoningContentBlockDelta::Text(t),
                                )) if !t.is_empty() => {
                                    yield Ok(make_reasoning_chunk(
                                        &message_id,
                                        &model_id,
                                        t.clone(),
                                    ));
                                }
                                _ => {}
                            }
                        }
                        ConverseStreamOutput::MessageStop(e) => {
                            stop_reason = Some(map_stop_reason(e.stop_reason()));
                        }
                        ConverseStreamOutput::Metadata(e) => {
                            if let Some(u) = e.usage() {
                                usage = Some(convert_usage(u));
                            }
                        }
                        _ => {}
                    },
                    Ok(None) => break,
                    Err(e) => {
                        yield Err(aws_err(e));
                        return;
                    }
                }
            }

            yield Ok(make_final_chunk(&message_id, &model_id, stop_reason.take(), usage.take()));
        };

        Ok(Box::pin(chunk_stream))
    }
}

// ── Request translation: OpenAI → Converse ────────────────────────────────────

/// A fully-translated Converse request, ready to feed the SDK builder.
struct ConverseRequest {
    system: Vec<SystemContentBlock>,
    messages: Vec<Message>,
    inference: Option<InferenceConfiguration>,
    tools: Option<ToolConfiguration>,
}

fn build_converse_request(
    req: &ChatCompletionRequest,
    model_id: &str,
) -> Result<ConverseRequest, ProxyError> {
    Ok(ConverseRequest {
        system: build_system(req),
        messages: build_messages(req)?,
        inference: Some(build_inference(req, model_id)),
        tools: build_tool_config(req, model_id)?,
    })
}

/// System prompt(s) → Converse `system` blocks. Both the dedicated `system`
/// field and any `role: "system"` messages contribute a block, in order.
fn build_system(req: &ChatCompletionRequest) -> Vec<SystemContentBlock> {
    let mut blocks = Vec::new();
    if let Some(s) = &req.system {
        if !s.is_empty() {
            blocks.push(SystemContentBlock::Text(s.clone()));
        }
    }
    for m in &req.messages {
        if m.role == "system" {
            let text = text_of(&m.content);
            if !text.is_empty() {
                blocks.push(SystemContentBlock::Text(text));
            }
        }
    }
    blocks
}

/// Build the Converse `messages` list. Converse requires strictly alternating
/// user/assistant turns, so adjacent same-role blocks are coalesced into one
/// message. `role: "tool"` results become a `toolResult` block on a **user**
/// turn; assistant `tool_calls` become `toolUse` blocks.
fn build_messages(req: &ChatCompletionRequest) -> Result<Vec<Message>, ProxyError> {
    // (role, blocks) entries, coalescing adjacent same-role entries.
    let mut entries: Vec<(ConversationRole, Vec<ContentBlock>)> = Vec::new();
    let mut push = |role: ConversationRole, blocks: Vec<ContentBlock>| {
        if blocks.is_empty() {
            return;
        }
        match entries.last_mut() {
            Some((last_role, last_blocks)) if *last_role == role => last_blocks.extend(blocks),
            _ => entries.push((role, blocks)),
        }
    };

    for m in &req.messages {
        match m.role.as_str() {
            "system" => {} // handled by build_system
            "tool" => {
                let result = text_of(&m.content);
                let block = ToolResultBlock::builder()
                    .tool_use_id(m.tool_call_id.clone().unwrap_or_default())
                    .content(ToolResultContentBlock::Text(result))
                    .build()
                    .map_err(build_err)?;
                push(
                    ConversationRole::User,
                    vec![ContentBlock::ToolResult(block)],
                );
            }
            "assistant" => {
                let mut blocks = Vec::new();
                if let Some(MessageContent::Text(t)) = &m.content {
                    if !t.is_empty() {
                        blocks.push(ContentBlock::Text(t.clone()));
                    }
                }
                if let Some(tcs) = &m.tool_calls {
                    for tc in tcs {
                        let input: Value = serde_json::from_str(&tc.function.arguments)
                            .unwrap_or_else(|_| serde_json::json!({}));
                        let block = ToolUseBlock::builder()
                            .tool_use_id(tc.id.clone())
                            .name(tc.function.name.clone())
                            .input(json_to_document(&input))
                            .build()
                            .map_err(build_err)?;
                        blocks.push(ContentBlock::ToolUse(block));
                    }
                }
                // An assistant turn with neither text nor tool calls is dropped:
                // Converse rejects empty-content messages.
                push(ConversationRole::Assistant, blocks);
            }
            _ => {
                // user (and any unknown role treated as user)
                push(ConversationRole::User, user_content_blocks(&m.content));
            }
        }
    }

    entries
        .into_iter()
        .map(|(role, blocks)| {
            Message::builder()
                .role(role)
                .set_content(Some(blocks))
                .build()
                .map_err(build_err)
        })
        .collect()
}

/// A user message's content → Converse content blocks (text + images).
fn user_content_blocks(content: &Option<MessageContent>) -> Vec<ContentBlock> {
    match content {
        Some(MessageContent::Text(t)) => {
            if t.is_empty() {
                Vec::new()
            } else {
                vec![ContentBlock::Text(t.clone())]
            }
        }
        Some(MessageContent::Parts(parts)) => parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text { text } if !text.is_empty() => {
                    Some(ContentBlock::Text(text.clone()))
                }
                ContentPart::Text { .. } => None,
                ContentPart::ImageUrl { image_url } => image_block(&image_url.url),
            })
            .collect(),
        None => Vec::new(),
    }
}

/// Build a Converse `image` block from an OpenAI image URL. Converse only
/// accepts inline image **bytes**, so `data:` base64 URLs are decoded here;
/// remote `http(s)` URLs are not fetched (unsupported by Converse directly) and
/// are skipped with a warning.
fn image_block(url: &str) -> Option<ContentBlock> {
    let (media_type, bytes) = match parse_data_url(url) {
        Some(v) => v,
        None => {
            tracing::warn!(
                "Bedrock Converse only supports inline image bytes; skipping non-data image URL"
            );
            return None;
        }
    };
    let block = ImageBlock::builder()
        .format(image_format(&media_type))
        .source(ImageSource::Bytes(Blob::new(bytes)))
        .build()
        .ok()?;
    Some(ContentBlock::Image(block))
}

/// Decode a `data:<media>;base64,<data>` URL into (media_type, bytes).
fn parse_data_url(url: &str) -> Option<(String, Vec<u8>)> {
    let rest = url.strip_prefix("data:")?;
    let (header, data) = rest.split_once(',')?;
    let media_type = header.split(';').next().unwrap_or("image/jpeg").to_string();
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data.trim())
        .ok()?;
    Some((media_type, bytes))
}

fn image_format(media_type: &str) -> ImageFormat {
    match media_type.trim().to_ascii_lowercase().as_str() {
        "image/png" => ImageFormat::Png,
        "image/gif" => ImageFormat::Gif,
        "image/webp" => ImageFormat::Webp,
        _ => ImageFormat::Jpeg,
    }
}

/// Build the Converse `inferenceConfig`.
///
/// Some Claude models reject `temperature` and `topP` set simultaneously, so
/// `topP` is dropped whenever a temperature is also present for a Claude model
/// (temperature takes precedence).
fn build_inference(req: &ChatCompletionRequest, model_id: &str) -> InferenceConfiguration {
    let mut ic = InferenceConfiguration::builder();
    ic = ic.max_tokens(req.max_tokens.unwrap_or(4096) as i32);
    if let Some(t) = req.temperature {
        ic = ic.temperature(t);
    }
    let drop_top_p = req.temperature.is_some() && model_id.contains("claude");
    if let Some(p) = req.top_p {
        if !drop_top_p {
            ic = ic.top_p(p);
        }
    }
    if let Some(stop) = &req.stop {
        let seqs = match stop {
            StopSequences::Single(v) => vec![v.clone()],
            StopSequences::Multiple(v) => v.clone(),
        };
        for s in seqs {
            ic = ic.stop_sequences(s);
        }
    }
    ic.build()
}

/// Build the Converse `toolConfig` from OpenAI `tools` + `tool_choice`.
fn build_tool_config(
    req: &ChatCompletionRequest,
    model_id: &str,
) -> Result<Option<ToolConfiguration>, ProxyError> {
    let Some(tools) = &req.tools else {
        return Ok(None);
    };
    if tools.is_empty() {
        return Ok(None);
    }

    let mut tc = ToolConfiguration::builder();
    for t in tools {
        let schema = t
            .function
            .parameters
            .clone()
            .unwrap_or_else(|| serde_json::json!({"type": "object"}));
        let mut spec = ToolSpecification::builder()
            .name(t.function.name.clone())
            .input_schema(ToolInputSchema::Json(json_to_document(&schema)));
        if let Some(desc) = &t.function.description {
            spec = spec.description(desc.clone());
        }
        tc = tc.tools(Tool::ToolSpec(spec.build().map_err(build_err)?));
    }

    // Llama 3.1 on Bedrock does not support toolChoice; omit it there.
    if !model_id.contains("llama3-1") {
        if let Some(choice) = map_tool_choice(req.tool_choice.as_ref())? {
            tc = tc.tool_choice(choice);
        }
    }

    Ok(Some(tc.build().map_err(build_err)?))
}

/// Map an OpenAI `tool_choice` to a Converse `ToolChoice`.
fn map_tool_choice(tc: Option<&Value>) -> Result<Option<ToolChoice>, ProxyError> {
    let Some(tc) = tc else {
        return Ok(None);
    };
    match tc {
        Value::String(s) => match s.as_str() {
            "auto" => Ok(Some(ToolChoice::Auto(AutoToolChoice::builder().build()))),
            "required" => Ok(Some(ToolChoice::Any(AnyToolChoice::builder().build()))),
            // Converse has no explicit "none"; omit to let the model not call tools.
            _ => Ok(None),
        },
        Value::Object(o) => match o
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(|n| n.as_str())
        {
            Some(name) => Ok(Some(ToolChoice::Tool(
                SpecificToolChoice::builder()
                    .name(name.to_string())
                    .build()
                    .map_err(build_err)?,
            ))),
            None => Ok(None),
        },
        _ => Ok(None),
    }
}

// ── Response translation: Converse → OpenAI ───────────────────────────────────

fn converse_output_to_openai(
    output: Option<&ConverseOutput>,
    stop_reason: &StopReason,
    usage: Option<&TokenUsage>,
    model_id: &str,
) -> ChatCompletionResponse {
    let mut text = String::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();

    if let Some(ConverseOutput::Message(msg)) = output {
        for block in msg.content() {
            match block {
                ContentBlock::Text(t) => text.push_str(t),
                ContentBlock::ToolUse(tu) => tool_calls.push(ToolCall {
                    id: tu.tool_use_id().to_string(),
                    r#type: "function".to_string(),
                    function: FunctionCall {
                        name: tu.name().to_string(),
                        arguments: document_to_json(tu.input()).to_string(),
                    },
                }),
                _ => {}
            }
        }
    }

    let message = ChatMessage {
        role: "assistant".to_string(),
        content: if text.is_empty() {
            None
        } else {
            Some(MessageContent::Text(text))
        },
        name: None,
        tool_calls: if tool_calls.is_empty() {
            None
        } else {
            Some(tool_calls)
        },
        tool_call_id: None,
        reasoning_content: None,
    };

    ChatCompletionResponse {
        id: format!("chatcmpl-{}", Uuid::new_v4()),
        object: "chat.completion".to_string(),
        created: chrono::Utc::now().timestamp() as u64,
        model: model_id.to_string(),
        choices: vec![Choice {
            index: 0,
            message,
            finish_reason: Some(map_stop_reason(stop_reason)),
            extra: Default::default(),
        }],
        usage: usage.map(convert_usage),
        system_fingerprint: None,
        extra: Default::default(),
    }
}

/// Map a Converse `StopReason` to an OpenAI `finish_reason`.
fn map_stop_reason(reason: &StopReason) -> String {
    match reason {
        StopReason::EndTurn | StopReason::StopSequence => "stop",
        StopReason::ToolUse => "tool_calls",
        StopReason::MaxTokens => "length",
        StopReason::ContentFiltered | StopReason::GuardrailIntervened => "content_filter",
        _ => "stop",
    }
    .to_string()
}

fn convert_usage(u: &TokenUsage) -> Usage {
    let prompt = u.input_tokens().max(0) as u32;
    let completion = u.output_tokens().max(0) as u32;
    Usage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: prompt + completion,
        extra: Default::default(),
    }
}

// ── Document ↔ serde_json::Value ──────────────────────────────────────────────

/// Convert a `serde_json::Value` into an `aws_smithy_types::Document` (used for
/// tool input schemas and streamed tool arguments).
fn json_to_document(v: &Value) -> Document {
    match v {
        Value::Null => Document::Null,
        Value::Bool(b) => Document::Bool(*b),
        Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                Document::Number(Number::PosInt(u))
            } else if let Some(i) = n.as_i64() {
                Document::Number(Number::NegInt(i))
            } else {
                Document::Number(Number::Float(n.as_f64().unwrap_or(0.0)))
            }
        }
        Value::String(s) => Document::String(s.clone()),
        Value::Array(arr) => Document::Array(arr.iter().map(json_to_document).collect()),
        Value::Object(obj) => Document::Object(
            obj.iter()
                .map(|(k, v)| (k.clone(), json_to_document(v)))
                .collect(),
        ),
    }
}

/// Convert an `aws_smithy_types::Document` back into a `serde_json::Value`.
fn document_to_json(d: &Document) -> Value {
    match d {
        Document::Null => Value::Null,
        Document::Bool(b) => Value::Bool(*b),
        Document::Number(n) => match n {
            Number::PosInt(u) => Value::from(*u),
            Number::NegInt(i) => Value::from(*i),
            Number::Float(f) => serde_json::Number::from_f64(*f)
                .map(Value::Number)
                .unwrap_or(Value::Null),
        },
        Document::String(s) => Value::String(s.clone()),
        Document::Array(arr) => Value::Array(arr.iter().map(document_to_json).collect()),
        Document::Object(obj) => Value::Object(
            obj.iter()
                .map(|(k, v)| (k.clone(), document_to_json(v)))
                .collect(),
        ),
    }
}

fn build_err(e: aws_smithy_types::error::operation::BuildError) -> ProxyError {
    ProxyError::AwsError(format!("Bedrock request build failed: {e}"))
}

/// Turn an AWS SDK error into a `ProxyError` carrying the full modeled message.
///
/// `SdkError::to_string()` collapses service errors to a bare `"service error"`;
/// `DisplayErrorContext` walks the source chain so the actual Bedrock message
/// (e.g. `ResourceNotFoundException: ...`) reaches the client and the logs.
fn aws_err<E: std::error::Error + 'static>(e: E) -> ProxyError {
    ProxyError::AwsError(format!(
        "{}",
        aws_smithy_types::error::display::DisplayErrorContext(e)
    ))
}

/// Flatten a message's content to plain text (text parts concatenated).
fn text_of(content: &Option<MessageContent>) -> String {
    match content {
        Some(MessageContent::Text(t)) => t.clone(),
        Some(MessageContent::Parts(parts)) => parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(""),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ContentPart, ImageUrl, Tool, ToolFunction};

    fn req(
        messages: Vec<ChatMessage>,
        with_tools: bool,
        tool_choice: Option<Value>,
    ) -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: "claude".into(),
            messages,
            stream: None,
            temperature: None,
            max_tokens: None,
            top_p: None,
            stop: None,
            tools: with_tools.then(|| {
                vec![Tool {
                    r#type: "function".into(),
                    function: ToolFunction {
                        name: "get_weather".into(),
                        description: Some("w".into()),
                        parameters: Some(serde_json::json!({"type": "object"})),
                    },
                }]
            }),
            tool_choice,
            system: None,
            extra_headers: Default::default(),
            raw_anthropic_body: None,
            extra: Default::default(),
        }
    }

    fn text_msg(role: &str, text: &str) -> ChatMessage {
        ChatMessage {
            role: role.into(),
            content: Some(MessageContent::Text(text.into())),
            name: None,
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
        }
    }

    fn asst_tool_call() -> ChatMessage {
        ChatMessage {
            role: "assistant".into(),
            content: None,
            name: None,
            tool_calls: Some(vec![ToolCall {
                id: "t1".into(),
                r#type: "function".into(),
                function: FunctionCall {
                    name: "get_weather".into(),
                    arguments: r#"{"loc":"NYC"}"#.into(),
                },
            }]),
            tool_call_id: None,
            reasoning_content: None,
        }
    }

    #[test]
    fn consecutive_same_role_messages_are_merged() {
        // Two user turns in a row must coalesce into one Converse message.
        let msgs = vec![
            text_msg("user", "hello"),
            text_msg("user", "world"),
            text_msg("assistant", "hi"),
        ];
        let out = build_messages(&req(msgs, false, None)).unwrap();
        assert_eq!(out.len(), 2, "adjacent user turns merged");
        assert_eq!(*out[0].role(), ConversationRole::User);
        assert_eq!(out[0].content().len(), 2);
        assert_eq!(*out[1].role(), ConversationRole::Assistant);
    }

    #[test]
    fn tool_result_becomes_user_tool_result_block() {
        let tool_msg = ChatMessage {
            role: "tool".into(),
            content: Some(MessageContent::Text("sunny".into())),
            name: None,
            tool_calls: None,
            tool_call_id: Some("t1".into()),
            reasoning_content: None,
        };
        let out = build_messages(&req(vec![asst_tool_call(), tool_msg], false, None)).unwrap();
        assert_eq!(out.len(), 2);
        // assistant tool_use
        assert_eq!(*out[0].role(), ConversationRole::Assistant);
        assert!(matches!(out[0].content()[0], ContentBlock::ToolUse(_)));
        // tool result on a user turn
        assert_eq!(*out[1].role(), ConversationRole::User);
        assert!(matches!(out[1].content()[0], ContentBlock::ToolResult(_)));
    }

    #[test]
    fn empty_assistant_message_is_dropped() {
        let msgs = vec![
            text_msg("user", "hi"),
            text_msg("assistant", ""),
            text_msg("user", "again"),
        ];
        let out = build_messages(&req(msgs, false, None)).unwrap();
        // The empty assistant turn vanishes, so the two user turns merge.
        assert_eq!(out.len(), 1);
        assert_eq!(*out[0].role(), ConversationRole::User);
    }

    #[test]
    fn system_field_and_role_become_system_blocks() {
        let mut r = req(vec![text_msg("system", "from message")], false, None);
        r.system = Some("from field".into());
        let blocks = build_system(&r);
        assert_eq!(blocks.len(), 2);
    }

    #[test]
    fn tools_and_tool_choice_map_to_config() {
        let cfg = build_tool_config(
            &req(
                vec![text_msg("user", "hi")],
                true,
                Some(serde_json::json!("required")),
            ),
            "claude",
        )
        .unwrap()
        .expect("tool config present");
        assert_eq!(cfg.tools().len(), 1);
        assert!(matches!(cfg.tool_choice(), Some(ToolChoice::Any(_))));
    }

    #[test]
    fn llama_3_1_omits_tool_choice() {
        let cfg = build_tool_config(
            &req(
                vec![text_msg("user", "hi")],
                true,
                Some(serde_json::json!("auto")),
            ),
            "meta.llama3-1-70b-instruct-v1:0",
        )
        .unwrap()
        .expect("tool config present");
        assert!(cfg.tool_choice().is_none(), "llama 3.1 drops tool_choice");
    }

    #[test]
    fn claude_drops_top_p_when_temperature_present() {
        let mut r = req(vec![text_msg("user", "hi")], false, None);
        r.temperature = Some(0.5);
        r.top_p = Some(0.9);
        let ic = build_inference(&r, "anthropic.claude-sonnet-4-5-20250929-v1:0");
        assert_eq!(ic.temperature(), Some(0.5));
        assert!(
            ic.top_p().is_none(),
            "topP dropped for claude when temp set"
        );
    }

    #[test]
    fn non_claude_keeps_top_p_with_temperature() {
        let mut r = req(vec![text_msg("user", "hi")], false, None);
        r.temperature = Some(0.5);
        r.top_p = Some(0.9);
        let ic = build_inference(&r, "amazon.nova-pro-v1:0");
        assert_eq!(ic.top_p(), Some(0.9));
    }

    #[test]
    fn image_data_url_becomes_image_block() {
        let m = ChatMessage {
            role: "user".into(),
            content: Some(MessageContent::Parts(vec![
                ContentPart::Text {
                    text: "look".into(),
                },
                ContentPart::ImageUrl {
                    image_url: ImageUrl {
                        url: "data:image/png;base64,QUJD".into(),
                        detail: None,
                    },
                },
            ])),
            name: None,
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
        };
        let blocks = user_content_blocks(&m.content);
        assert_eq!(blocks.len(), 2);
        assert!(matches!(blocks[0], ContentBlock::Text(_)));
        match &blocks[1] {
            ContentBlock::Image(img) => {
                assert_eq!(*img.format(), ImageFormat::Png);
            }
            _ => panic!("expected image block"),
        }
    }

    #[test]
    fn remote_image_url_is_skipped() {
        let blocks =
            user_content_blocks(&Some(MessageContent::Parts(vec![ContentPart::ImageUrl {
                image_url: ImageUrl {
                    url: "https://example.com/cat.png".into(),
                    detail: None,
                },
            }])));
        assert!(
            blocks.is_empty(),
            "remote image URL skipped (Converse needs bytes)"
        );
    }

    #[test]
    fn response_tool_use_becomes_tool_calls() {
        let msg = Message::builder()
            .role(ConversationRole::Assistant)
            .content(ContentBlock::Text("let me check".into()))
            .content(ContentBlock::ToolUse(
                ToolUseBlock::builder()
                    .tool_use_id("tu1")
                    .name("get_weather")
                    .input(json_to_document(&serde_json::json!({"loc": "NYC"})))
                    .build()
                    .unwrap(),
            ))
            .build()
            .unwrap();
        let out = converse_output_to_openai(
            Some(&ConverseOutput::Message(msg)),
            &StopReason::ToolUse,
            None,
            "claude",
        );
        let tc = &out.choices[0].message.tool_calls.as_ref().unwrap()[0];
        assert_eq!(tc.function.name, "get_weather");
        assert!(tc.function.arguments.contains("NYC"));
        assert_eq!(out.choices[0].finish_reason.as_deref(), Some("tool_calls"));
    }

    #[test]
    fn stop_reason_mapping() {
        assert_eq!(map_stop_reason(&StopReason::EndTurn), "stop");
        assert_eq!(map_stop_reason(&StopReason::MaxTokens), "length");
        assert_eq!(map_stop_reason(&StopReason::ToolUse), "tool_calls");
        assert_eq!(map_stop_reason(&StopReason::StopSequence), "stop");
        assert_eq!(
            map_stop_reason(&StopReason::ContentFiltered),
            "content_filter"
        );
    }

    #[test]
    fn document_json_roundtrip() {
        let v = serde_json::json!({
            "s": "x", "n": 3, "neg": -2, "f": 1.5, "b": true,
            "nil": null, "arr": [1, 2], "obj": {"k": "v"}
        });
        let round = document_to_json(&json_to_document(&v));
        assert_eq!(round, v);
    }

    #[test]
    fn usage_conversion() {
        let u = TokenUsage::builder()
            .input_tokens(5)
            .output_tokens(3)
            .total_tokens(8)
            .build()
            .unwrap();
        let got = convert_usage(&u);
        assert_eq!(got.prompt_tokens, 5);
        assert_eq!(got.completion_tokens, 3);
        assert_eq!(got.total_tokens, 8);
    }
}
