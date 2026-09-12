//! What a sub-request is made of: the system prompt, the session's project
//! instructions (stored in the meta), the history as stored, the tools, and
//! the provider's wire profile.
//!
//! Pure in `(provider, meta, history)`: no client, no session file, no clock.
//! That is the cache contract made visible — the same three inputs must produce
//! the same bytes, because the backend's prefix cache matches on them.

use serde_json::json;

use crate::api;
use crate::machine;
use crate::provider::{self, Wire};
use crate::session::SessionMeta;
use crate::tools;
use crate::types::{Message, ToolDef, WireRequest};

/// Participates in the request prefix (KVCache). Injecting time, cwd, a random
/// id or any other dynamic content is forbidden, or every request would have a
/// different prefix and the cache would miss entirely.
pub const SYSTEM_PROMPT: &str = "You are caocli, a coding agent. You and the user share one workspace, and your job is to collaborate with them until their goal is genuinely handled. Keep answers concise. Tool routing: use Read to read a file, Edit to modify an existing file, Write to create or fully rewrite a file, and Bash for everything else (running programs, builds, tests, git, directories, bulk text processing). Prefer absolute paths: each Bash call starts a fresh shell, so cd does not persist.";

/// Build the sub-request for a history: the system prompt, the history as
/// stored, the tools, and the provider's wire profile.
///
/// A free function rather than a method so its shape is testable without an
/// Agent behind it — no client, no session file: it is pure in
/// `(provider, meta, history)`. That is also the cache contract made visible:
/// the same three inputs must produce the same bytes, because the backend's
/// prefix cache matches on them.
pub fn build_request(
    provider: &provider::Provider,
    meta: &SessionMeta,
    history: &[Message],
) -> WireRequest {
    match provider.wire {
        Wire::OpenAi => WireRequest::OpenAi(Box::new(openai_request(provider, meta, history))),
        Wire::Anthropic => {
            WireRequest::Anthropic(Box::new(anthropic_request(provider, meta, history)))
        }
    }
}

/// The OpenAI chat-completions shape, built in the `openai` crate's vocabulary:
/// the request is a `ChatCompletionRequest`, the history is `ChatCompletionMessageParam`,
/// the tools are `Tool::function(FunctionDefinition)`. The vendor switches the
/// SDK does not model (`thinking`, `reasoning_effort`) are written through the
/// SDK's `extra_body` passthrough in [`api::openai_request`].
fn openai_request(
    provider: &provider::Provider,
    meta: &SessionMeta,
    history: &[Message],
) -> openai::ChatCompletionRequest {
    let mut messages = Vec::with_capacity(history.len() + 2);
    messages.push(Message::system(SYSTEM_PROMPT));
    // The session's project instructions, frozen into the meta at creation and
    // sent byte-for-byte from there — the prefix they open stays stable for
    // the life of the session, and nothing here reads the filesystem (the
    // purity of `(provider, meta, history)` is the cache contract). A user
    // message rather than a second system one: it is the one shape every
    // backend in the preset table takes, and the instructions are context the
    // model reads, not a voice it speaks in.
    if let Some(text) = meta.instructions.as_deref().filter(|t| !t.is_empty()) {
        messages.push(Message::user(text));
    }
    // A message that arrived on the Anthropic wire carries its thinking
    // blocks with it; those are that wire's business and are stripped here,
    // so a session that switched providers never sends them to a backend
    // that rejects fields it does not know.
    messages.extend(history.iter().map(|m| {
        let mut m = m.clone();
        m.thinking = None;
        m
    }));
    // Specification tripwire (debug builds only): the history being sent must
    // satisfy the executable specification. A violation is a shape the
    // backend answers with a 400 — catch it during development rather than in
    // production.
    debug_assert!(
        machine::is_request_valid(&messages),
        "request history violates the tool_calls window specification: {messages:?}"
    );
    // The thinking switch and the effort fallback are the provider's:
    // a preset that omits one or defaults differently says so in the
    // table, and nothing here needs to know which.
    api::openai_request(
        &meta.model,
        provider.max_tokens,
        &messages,
        Some(&tools::definitions()),
        Some("auto"),
        provider.send_thinking,
        Some(
            meta.reasoning_effort
                .as_deref()
                .unwrap_or(provider.default_effort),
        ),
    )
}

/// The Anthropic messages shape, built in the `anthropic` crate's vocabulary:
/// the system prompt is a top-level field, the history is content blocks, a
/// tool call is a `tool_use` block and its result a `tool_result` block in the
/// user turn that follows.
fn anthropic_request(
    provider: &provider::Provider,
    meta: &SessionMeta,
    history: &[Message],
) -> anthropic::MessagesRequest {
    // Specification tripwire, same as the OpenAI shape: the window
    // specification is wire-agnostic, so it is checked before the mapping.
    debug_assert!(
        machine::is_request_valid(history),
        "request history violates the tool_calls window specification: {history:?}"
    );
    let mut messages: Vec<anthropic::MessageParam> = Vec::with_capacity(history.len());
    // The system prompt rides on the request, not in the log, and the session's
    // project instructions ride with it: they are frozen into the meta at
    // creation and sent byte-for-byte from there, so the prefix they open stays
    // stable for the life of the session. A system message that somehow got
    // into the history folds in here rather than being dropped.
    let mut system = vec![SYSTEM_PROMPT.to_string()];
    if let Some(text) = meta.instructions.as_deref().filter(|t| !t.is_empty()) {
        system.push(text.to_string());
    }
    for msg in history {
        match msg.role {
            crate::types::Role::System => {
                if let Some(text) = msg.text() {
                    system.push(text);
                }
            }
            crate::types::Role::User => messages.push(anthropic::MessageParam {
                role: anthropic::Role::User,
                content: user_content(msg),
            }),
            crate::types::Role::Assistant => messages.push(assistant_message(msg)),
            crate::types::Role::Tool => {
                // Results ride in the user turn that follows the calls, so
                // consecutive results fold into one user message of
                // tool_result blocks — the shape the wire expects, and one
                // the window specification above guarantees is well placed.
                let block = tool_result_block(msg);
                match messages.last_mut() {
                    Some(anthropic::MessageParam {
                        role: anthropic::Role::User,
                        content: anthropic::MessageContent::Blocks(blocks),
                    }) if blocks
                        .iter()
                        .all(|b| matches!(b.kind, anthropic::BlockKind::ToolResult { .. })) =>
                    {
                        blocks.push(block);
                    }
                    _ => messages.push(anthropic::MessageParam::blocks(
                        anthropic::Role::User,
                        vec![block],
                    )),
                }
            }
        }
    }
    let mut request =
        anthropic::MessagesRequest::new(meta.model.clone(), provider.max_tokens, messages)
            .streaming()
            .with_system(system.join("\n\n"))
            .with_client_tools(
                tools::definitions()
                    .into_iter()
                    .map(anthropic_tool)
                    .collect(),
            )
            .with_tool_choice(anthropic::ToolChoice::auto())
            .with_thinking(anthropic_thinking(provider, meta));
    // The standard optional fields, each one the preset's to offer. See
    // `provider::AnthropicOptions` for why they are not simply always sent.
    if provider.anthropic.effort
        && let Some(effort) = anthropic::Effort::from_name(&effort_in_force(provider, meta))
    {
        request = request.with_effort(effort);
    }
    if provider.anthropic.cache_control {
        // The automatic breakpoint: one field, and the server keeps the
        // breakpoint at the end of the cacheable prefix itself, which is the
        // form that suits a conversation that only ever grows.
        request = request.with_automatic_cache();
    }
    request
}

/// The thinking configuration this provider gets.
///
/// The effort slot is the thinking switch on this wire — MiniMax M3 has no
/// effort tiers, only thinking on (`adaptive`) and off — unless the preset says
/// the endpoint serves the standard effort field, in which case the tier is
/// that field's business and thinking is simply on.
///
/// `display` rides with it when the preset says the endpoint takes it: the
/// field defaults to `omitted` on the models that have adaptive thinking, where
/// the answer carries a signature and no words at all.
fn anthropic_thinking(
    provider: &provider::Provider,
    meta: &SessionMeta,
) -> anthropic::ThinkingConfig {
    let thinking = match (
        provider.anthropic.effort,
        effort_in_force(provider, meta).as_str(),
    ) {
        (true, _) => anthropic::ThinkingConfig::adaptive(),
        (false, "off") => anthropic::ThinkingConfig::disabled(),
        (false, _) => anthropic::ThinkingConfig::adaptive(),
    };
    if provider.anthropic.display {
        thinking.summarized()
    } else {
        thinking
    }
}

/// The tier in force: the one the session stored, or the provider's default —
/// the same fallback the status line reports.
fn effort_in_force(provider: &provider::Provider, meta: &SessionMeta) -> String {
    meta.reasoning_effort
        .clone()
        .unwrap_or_else(|| provider.default_effort.to_string())
}

/// A user message's content: the string it is, or the blocks its parts become.
/// A turn that is one run of text stays the string it was — that is the form
/// the spec writes for it, and the form the prefix cache hashes.
fn user_content(msg: &Message) -> anthropic::MessageContent {
    match &msg.content {
        Some(crate::types::Content::Text(text)) => anthropic::MessageContent::Text(text.clone()),
        Some(crate::types::Content::Parts(parts)) => anthropic::MessageContent::Blocks(
            parts
                .iter()
                .map(|p| match (&p.text, &p.image_url) {
                    (Some(text), _) => anthropic::Block::text(text),
                    (None, Some(image)) => anthropic::Block::image(image_source(&image.url)),
                    (None, None) => anthropic::Block::text(""),
                })
                .collect(),
        ),
        None => anthropic::MessageContent::Text(String::new()),
    }
}

/// An assistant message as blocks, in the order the model wrote them: thinking
/// (replayed verbatim, signature included), then text, then the calls it
/// declared. `reasoning_content` — the OpenAI-wire spelling of the same
/// reasoning — is deliberately not replayed: without the signature that came
/// with it on this wire it would not be the content the model returned, and a
/// made-up signature would be worse than none.
fn assistant_message(msg: &Message) -> anthropic::MessageParam {
    let mut blocks: Vec<anthropic::Block> = Vec::new();
    for block in msg.thinking.iter().flatten() {
        blocks.push(anthropic::Block::thinking(
            &block.thinking,
            &block.signature,
        ));
    }
    if let Some(text) = msg.text().filter(|text| !text.is_empty()) {
        blocks.push(anthropic::Block::text(text));
    }
    for call in msg.tool_calls.iter().flatten() {
        blocks.push(anthropic::Block::tool_use(
            &call.id,
            &call.function.name,
            serde_json::from_str(&call.function.arguments).unwrap_or_else(|_| json!({})),
        ));
    }
    anthropic::MessageParam::blocks(anthropic::Role::Assistant, blocks)
}

fn tool_result_block(msg: &Message) -> anthropic::Block {
    let text = msg.text().unwrap_or_default();
    anthropic::Block::tool_result_full(
        msg.tool_call_id.clone().unwrap_or_default(),
        anthropic::ToolResultContent::Text(text.clone()),
        // A failure says so in its text, and this wire has a field for it: a
        // failure reported as an ordinary result reads to the model as an
        // answer to work from rather than a tool that did not do its job.
        tools::reports_failure(&text),
    )
}

/// An image as the wire reads it: a `data:` URL carries the bytes themselves
/// and becomes the base64 source (media type from the URL); anything else is
/// somewhere to be fetched from, and becomes a URL source.
fn image_source(url: &str) -> anthropic::ImageSource {
    url.strip_prefix("data:")
        .and_then(|rest| rest.split_once(";base64,"))
        .map(|(media_type, data)| anthropic::ImageSource::Base64 {
            media_type: media_type.to_string(),
            data: data.to_string(),
        })
        .unwrap_or_else(|| anthropic::ImageSource::Url {
            url: url.to_string(),
        })
}

fn anthropic_tool(def: ToolDef) -> anthropic::Tool {
    let mut tool = anthropic::Tool::new(
        def.function.name,
        def.function.description.unwrap_or_default(),
        def.function
            .parameters
            .unwrap_or_else(|| json!({"type": "object"})),
    );
    if tool.description.as_deref() == Some("") {
        tool.description = None;
    }
    tool
}

#[cfg(test)]
mod tests;
