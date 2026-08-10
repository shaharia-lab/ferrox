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
    AnyToolChoice, AutoToolChoice, CachePointBlock, CachePointType, ContentBlock,
    ContentBlockDelta, ContentBlockStart, ConversationRole, ConverseOutput, ConverseStreamOutput,
    ImageBlock, ImageFormat, ImageSource, InferenceConfiguration, Message,
    ReasoningContentBlockDelta, SpecificToolChoice, StopReason, SystemContentBlock, TokenUsage,
    Tool, ToolChoice, ToolConfiguration, ToolInputSchema, ToolResultBlock, ToolResultContentBlock,
    ToolSpecification, ToolUseBlock,
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
    let mut system = build_system(req);
    let mut entries = build_message_entries(req)?;
    apply_cache_point_policy(&mut system, &mut entries, model_id);

    let messages = entries
        .into_iter()
        .map(|(role, blocks)| {
            Message::builder()
                .role(role)
                .set_content(Some(blocks))
                .build()
                .map_err(build_err)
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(ConverseRequest {
        system,
        messages,
        inference: Some(build_inference(req, model_id)),
        tools: build_tool_config(req, model_id)?,
    })
}

// ── Prompt caching (`cachePoint`) ────────────────────────────────────────────

/// Bedrock accepts at most this many cache points per request.
const MAX_CACHE_POINTS: usize = 4;

/// A `cachePoint` block. The only required field is the type, which is always
/// set here, so construction cannot fail.
fn cache_point() -> CachePointBlock {
    CachePointBlock::builder()
        .r#type(CachePointType::Default)
        .build()
        .expect("CachePointBlock requires only `type`, which is set above")
}

/// Whether this model family accepts `cachePoint` blocks.
///
/// An **allowlist**, deliberately: a family that does not support caching
/// rejects the block outright, turning a would-be optimisation into a hard
/// request failure. An unrecognised model must therefore default to *not*
/// sending one.
fn supports_cache_points(model_id: &str) -> bool {
    let id = model_id.to_ascii_lowercase();
    id.contains("anthropic.claude") || id.contains("amazon.nova")
}

/// Enforce the capability gate and the 4-point cap across the whole request.
///
/// When more than four breakpoints are requested the **last** four are kept:
/// caching is prefix-based, so a later breakpoint covers everything an earlier
/// one would have, and is strictly more valuable.
fn apply_cache_point_policy(
    system: &mut Vec<SystemContentBlock>,
    entries: &mut [(ConversationRole, Vec<ContentBlock>)],
    model_id: &str,
) {
    let total = system
        .iter()
        .filter(|b| matches!(b, SystemContentBlock::CachePoint(_)))
        .count()
        + entries
            .iter()
            .flat_map(|(_, blocks)| blocks.iter())
            .filter(|b| matches!(b, ContentBlock::CachePoint(_)))
            .count();

    if total == 0 {
        return;
    }

    let mut to_drop = if supports_cache_points(model_id) {
        if total <= MAX_CACHE_POINTS {
            return;
        }
        tracing::debug!(
            requested = total,
            kept = MAX_CACHE_POINTS,
            "more cache points than Bedrock allows; keeping the last {MAX_CACHE_POINTS}"
        );
        total - MAX_CACHE_POINTS
    } else {
        tracing::debug!(
            model_id,
            dropped = total,
            "model family does not support Bedrock cache points; omitting them"
        );
        total
    };

    // Drop from the front, in request order: system blocks precede messages.
    system.retain(|b| {
        if to_drop > 0 && matches!(b, SystemContentBlock::CachePoint(_)) {
            to_drop -= 1;
            false
        } else {
            true
        }
    });
    for (_, blocks) in entries.iter_mut() {
        blocks.retain(|b| {
            if to_drop > 0 && matches!(b, ContentBlock::CachePoint(_)) {
                to_drop -= 1;
                false
            } else {
                true
            }
        });
    }
}

/// System prompt(s) → Converse `system` blocks. Both the dedicated `system`
/// field and any `role: "system"` messages contribute a block, in order.
fn build_system(req: &ChatCompletionRequest) -> Vec<SystemContentBlock> {
    let mut blocks = Vec::new();
    if let Some(s) = &req.system {
        if !s.is_empty() {
            blocks.push(SystemContentBlock::Text(s.clone()));
            // A breakpoint on the system prompt arrives at request level (the
            // internal `system` is a flat string) — see `types::
            // ANTHROPIC_SYSTEM_CACHE_CONTROL`. Terminate the span here.
            if req
                .extra
                .contains_key(crate::types::ANTHROPIC_SYSTEM_CACHE_CONTROL)
            {
                blocks.push(SystemContentBlock::CachePoint(cache_point()));
            }
        }
    }
    for m in &req.messages {
        if m.role == "system" {
            let text = text_of(&m.content);
            if !text.is_empty() {
                blocks.push(SystemContentBlock::Text(text));
                if m.extra.contains_key(crate::types::CACHE_CONTROL) {
                    blocks.push(SystemContentBlock::CachePoint(cache_point()));
                }
            }
        }
    }
    blocks
}

/// Build the Converse `messages` list. Converse requires strictly alternating
/// user/assistant turns, so adjacent same-role blocks are coalesced into one
/// message. `role: "tool"` results become a `toolResult` block on a **user**
/// turn; assistant `tool_calls` become `toolUse` blocks.
#[allow(clippy::type_complexity)]
fn build_message_entries(
    req: &ChatCompletionRequest,
) -> Result<Vec<(ConversationRole, Vec<ContentBlock>)>, ProxyError> {
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

    Ok(entries)
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
            .flat_map(|p| {
                // A cache point terminates the span it applies to, so it is
                // emitted immediately *after* the block carrying the breakpoint.
                let (block, extra) = match p {
                    ContentPart::Text { text, extra } if !text.is_empty() => {
                        (Some(ContentBlock::Text(text.clone())), extra)
                    }
                    ContentPart::Text { extra, .. } => (None, extra),
                    ContentPart::ImageUrl { image_url, extra } => {
                        (image_block(&image_url.url), extra)
                    }
                };
                // A breakpoint only means anything if a block was actually
                // emitted for it to terminate.
                let breakpoint = block.is_some() && extra.contains_key(crate::types::CACHE_CONTROL);
                block
                    .into_iter()
                    .chain(breakpoint.then(|| ContentBlock::CachePoint(cache_point())))
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
    // OpenAI `tool_choice: "none"` means the model must not call a tool. Converse
    // has no "none", so omit the whole toolConfig: with no tools advertised the
    // model can't call one, which is the faithful behaviour for this turn.
    if req.tool_choice.as_ref().and_then(|v| v.as_str()) == Some("none") {
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
        extra: Default::default(),
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
    // Bedrock names them cacheRead/cacheWrite; they carry the same meaning as
    // Anthropic's cache_read/cache_creation and must land in the same
    // `Usage.extra` shape, or the observability layer would have two sources.
    let cache_read = u.cache_read_input_tokens().map(|t| t.max(0) as u32);
    let cache_write = u.cache_write_input_tokens().map(|t| t.max(0) as u32);
    Usage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: prompt + completion,
        extra: crate::types::cache_usage_extra(cache_write, cache_read),
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
                ContentPart::Text { text, .. } => Some(text.as_str()),
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

    /// Entries → `Message`s, mirroring what `build_converse_request` does, so
    /// message-shape tests can assert on the built SDK type.
    fn build_messages(req: &ChatCompletionRequest) -> Result<Vec<Message>, ProxyError> {
        Ok(build_message_entries(req)?
            .into_iter()
            .map(|(role, blocks)| {
                Message::builder()
                    .role(role)
                    .set_content(Some(blocks))
                    .build()
                    .expect("role and content are set")
            })
            .collect())
    }

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
            extra: Default::default(),
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
            extra: Default::default(),
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
            extra: Default::default(),
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
    fn tool_choice_none_omits_tool_config() {
        // "none" must suppress tool use — no toolConfig is sent at all.
        let cfg = build_tool_config(
            &req(
                vec![text_msg("user", "hi")],
                true,
                Some(serde_json::json!("none")),
            ),
            "claude",
        )
        .unwrap();
        assert!(cfg.is_none(), "tool_choice=none drops the tool config");
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
                    extra: Default::default(),
                },
                ContentPart::ImageUrl {
                    image_url: ImageUrl {
                        url: "data:image/png;base64,QUJD".into(),
                        detail: None,
                    },
                    extra: Default::default(),
                },
            ])),
            name: None,
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            extra: Default::default(),
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
                extra: Default::default(),
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
        assert!(
            got.extra.is_empty(),
            "no cache fields upstream must mean no extra keys: {:?}",
            got.extra
        );
    }

    // ── Prompt caching: read side ────────────────────────────────────────────

    #[test]
    fn usage_conversion_surfaces_cache_counters() {
        let u = TokenUsage::builder()
            .input_tokens(47)
            .output_tokens(2)
            .total_tokens(49)
            .cache_read_input_tokens(3968)
            .cache_write_input_tokens(100)
            .build()
            .unwrap();
        let got = convert_usage(&u);
        // Same key shape as the Anthropic adapter (#125), so the observability
        // layer has exactly one representation to read.
        assert_eq!(got.extra["cache_read_input_tokens"], 3968);
        assert_eq!(got.extra["cache_creation_input_tokens"], 100);
        assert_eq!(got.extra["prompt_tokens_details"]["cached_tokens"], 3968);
    }

    // ── Prompt caching: write side ───────────────────────────────────────────

    fn ephemeral() -> std::collections::HashMap<String, Value> {
        std::collections::HashMap::from([(
            "cache_control".to_string(),
            serde_json::json!({"type": "ephemeral"}),
        )])
    }

    /// A user message whose Nth text part carries a breakpoint.
    fn msg_with_breakpoints(texts: &[(&str, bool)]) -> ChatMessage {
        ChatMessage {
            role: "user".into(),
            content: Some(MessageContent::Parts(
                texts
                    .iter()
                    .map(|(t, cached)| ContentPart::Text {
                        text: (*t).into(),
                        extra: if *cached {
                            ephemeral()
                        } else {
                            Default::default()
                        },
                    })
                    .collect(),
            )),
            name: None,
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            extra: Default::default(),
        }
    }

    fn count_cache_points(built: &ConverseRequest) -> usize {
        built
            .system
            .iter()
            .filter(|b| matches!(b, SystemContentBlock::CachePoint(_)))
            .count()
            + built
                .messages
                .iter()
                .flat_map(|m| m.content().iter())
                .filter(|b| matches!(b, ContentBlock::CachePoint(_)))
                .count()
    }

    #[test]
    fn no_breakpoints_emit_no_cache_points() {
        let built = build_converse_request(
            &req(vec![text_msg("user", "hello")], false, None),
            "anthropic.claude-sonnet-4",
        )
        .unwrap();
        assert_eq!(count_cache_points(&built), 0);
        // Byte-identical to today: a plain text block and nothing else.
        assert_eq!(built.messages[0].content().len(), 1);
        assert!(matches!(
            built.messages[0].content()[0],
            ContentBlock::Text(_)
        ));
    }

    #[test]
    fn breakpoint_emits_cache_point_after_its_block() {
        let built = build_converse_request(
            &req(
                vec![msg_with_breakpoints(&[("cached", true), ("fresh", false)])],
                false,
                None,
            ),
            "anthropic.claude-sonnet-4",
        )
        .unwrap();

        let blocks = built.messages[0].content();
        assert_eq!(blocks.len(), 3, "text, cachePoint, text");
        assert!(matches!(blocks[0], ContentBlock::Text(_)));
        assert!(
            matches!(blocks[1], ContentBlock::CachePoint(_)),
            "cache point must terminate the span it applies to"
        );
        assert!(matches!(blocks[2], ContentBlock::Text(_)));
    }

    #[test]
    fn system_breakpoint_emits_a_system_cache_point() {
        let mut r = req(vec![text_msg("user", "hi")], false, None);
        r.system = Some("You are helpful.".into());
        r.extra.insert(
            crate::types::ANTHROPIC_SYSTEM_CACHE_CONTROL.to_string(),
            serde_json::json!({"type": "ephemeral"}),
        );

        let built = build_converse_request(&r, "anthropic.claude-sonnet-4").unwrap();
        assert_eq!(built.system.len(), 2, "text + cachePoint");
        assert!(matches!(built.system[0], SystemContentBlock::Text(_)));
        assert!(matches!(built.system[1], SystemContentBlock::CachePoint(_)));
    }

    #[test]
    fn more_than_four_breakpoints_keeps_the_last_four() {
        // Prefix caching makes later breakpoints strictly more valuable.
        let built = build_converse_request(
            &req(
                vec![msg_with_breakpoints(&[
                    ("a", true),
                    ("b", true),
                    ("c", true),
                    ("d", true),
                    ("e", true),
                    ("f", true),
                ])],
                false,
                None,
            ),
            "anthropic.claude-sonnet-4",
        )
        .unwrap();

        assert_eq!(count_cache_points(&built), MAX_CACHE_POINTS);
        // The survivors must be the *last* four: the first two texts have no
        // cache point following them.
        let blocks = built.messages[0].content();
        assert!(matches!(blocks[0], ContentBlock::Text(_)));
        assert!(matches!(blocks[1], ContentBlock::Text(_)));
        assert!(matches!(blocks[2], ContentBlock::Text(_)));
        assert!(matches!(blocks[3], ContentBlock::CachePoint(_)));
    }

    #[test]
    fn system_cache_point_is_dropped_first_when_over_cap() {
        // System blocks come first in request order, so they are the earliest
        // and therefore the least valuable to keep.
        let mut r = req(
            vec![msg_with_breakpoints(&[
                ("a", true),
                ("b", true),
                ("c", true),
                ("d", true),
            ])],
            false,
            None,
        );
        r.system = Some("sys".into());
        r.extra.insert(
            crate::types::ANTHROPIC_SYSTEM_CACHE_CONTROL.to_string(),
            serde_json::json!({"type": "ephemeral"}),
        );

        let built = build_converse_request(&r, "anthropic.claude-sonnet-4").unwrap();
        assert_eq!(count_cache_points(&built), MAX_CACHE_POINTS);
        assert_eq!(built.system.len(), 1, "system cache point dropped first");
        assert!(matches!(built.system[0], SystemContentBlock::Text(_)));
    }

    #[test]
    fn unsupported_model_family_emits_no_cache_points() {
        // Sending a cachePoint to a family that rejects it would turn an
        // optimisation into a hard 4xx.
        let built = build_converse_request(
            &req(vec![msg_with_breakpoints(&[("cached", true)])], false, None),
            "meta.llama3-70b-instruct-v1:0",
        )
        .unwrap();
        assert_eq!(count_cache_points(&built), 0);
        assert_eq!(built.messages[0].content().len(), 1, "text only");
    }

    #[test]
    fn cache_point_capability_allowlist() {
        assert!(supports_cache_points("anthropic.claude-sonnet-4-20250514"));
        assert!(supports_cache_points("us.anthropic.claude-3-5-haiku-v1:0"));
        assert!(supports_cache_points("amazon.nova-pro-v1:0"));
        // Unknown families must default to unsupported.
        assert!(!supports_cache_points("meta.llama3-70b-instruct-v1:0"));
        assert!(!supports_cache_points("mistral.mistral-large-2407-v1:0"));
        assert!(!supports_cache_points("some.future-model-v1:0"));
    }
}
