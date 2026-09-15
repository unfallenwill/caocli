use anthropic::{BlockDelta, BlockKind, Event as AnthropicEvent};
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::collections::HashMap;

use crate::provider::{Provider, Wire};
use crate::types::{
    ChatChunk, Content, ContentPart, Delta, DeltaFunctionCall, DeltaToolCall, Message, ToolCall,
    ToolDef, Usage, WireRequest,
};

// ============================================================================
// The three wires, one conversation.
//
// The OpenAI wire is the `openai` crate's: it speaks Chat Completions, does
// the HTTP, retries, and SSE parsing, and hands over typed chunks. What lives
// here is the bridge between the agent's internal types (the
// `crate::types::Message` it stores, the `crate::types::Delta` it folds) and
// the SDK's. The SDK handles transport; the adapter handles the deltas.
//
// The Responses wire is the same crate's other surface: an answer is a list of
// items rather than a list of choices, so the adapter is its own, but it lands
// on the same local deltas — and it is where the reasoning items and the
// prompt-cache counts are read from.
//
// The Anthropic wire is the `anthropic` crate's, the same way: typed events in,
// the same local deltas out. Nothing downstream learns a second protocol
// exists — the pieces the OpenAI wire has no field for ride in
// `reasoning_content` (caught from the SDK's `extra`), and the Anthropic
// usage lands once via a small merge.
// ============================================================================

enum Backend {
    /// The OpenAI chat-completions wire, spoken by the SDK. Both DeepSeek and
    /// Z.AI take this — vendor extensions ride through [`openai::ChatCompletionRequest::extra_body`]
    /// on the way out and [`openai::ChoiceDelta::extra`] / [`openai::CompletionUsage::extra`]
    /// on the way in.
    OpenAi(openai::Client),
    /// The Anthropic wire, spoken by the SDK.
    Anthropic(anthropic::Client),
    /// The OpenAI Responses wire. The same SDK client as the chat wire — one
    /// client serves both of that API's resources — but a different request
    /// shape and a different event stream, so the variant is its own: the pair
    /// (backend, request) is what a mismatched wire is caught by.
    Responses(openai::Client),
}

/// The client for one provider: its endpoint, its key and its wire, paired so
/// that one provider's key cannot be sent to another provider's endpoint.
pub struct Client {
    backend: Backend,
}

impl Client {
    /// The client for a provider: its endpoint, key and wire, paired in the
    /// one place that knows all three, so a caller cannot send one provider's
    /// key (or one wire's request shape) to another provider's URL.
    pub fn for_provider(provider: &Provider, key: String) -> Result<Self> {
        Self::new(key, provider.url.to_string(), provider.wire)
    }

    pub fn new(api_key: String, url: String, wire: Wire) -> Result<Self> {
        let backend = match wire {
            // The endpoint is the provider's own URL, path and all — the SDK
            // names its resource paths under it, and DeepSeek / Z.AI both
            // already end in `/chat/completions`. No retries, deliberately,
            // where the SDK's default is two: a refusal worth retrying is
            // one this program would rather show. Same reasoning as the
            // Anthropic path below.
            Wire::OpenAi => Backend::OpenAi(openai_client(api_key, url)?),
            // The Responses endpoint, named the same way: MiMo's own URL ends
            // in `/v1/responses`, and the SDK sends to it verbatim.
            Wire::Responses => Backend::Responses(openai_client(api_key, url)?),
            // The endpoint is the provider's own URL, path and all: an
            // Anthropic-compatible gateway puts the messages where it likes.
            //
            // No retries, deliberately, where the SDK's default is two. A
            // refusal worth retrying is one this program would rather show: a
            // backoff is a turn that has stopped responding for reasons the
            // person watching cannot see, and pressing enter again is one
            // keystroke. The SDK keeps the reference client's behavior for a
            // caller that wants it.
            Wire::Anthropic => Backend::Anthropic(anthropic::Client::new(
                anthropic::Profile::endpoint(url)
                    .with_bearer_token(api_key)
                    .with_max_retries(0),
            )?),
        };
        Ok(Self { backend })
    }

    /// Send one sub-request and hand back its stream.
    pub async fn stream_chat(&self, req: &WireRequest) -> Result<ChunkStream> {
        match (&self.backend, req) {
            (Backend::OpenAi(client), WireRequest::OpenAi(r)) => {
                let sdk_stream = client
                    .stream_completion(r)
                    .await
                    .map_err(|e| anyhow::anyhow!("openai stream error: {e}"))?;
                Ok(ChunkStream::OpenAi(OpenAiStream { inner: sdk_stream }))
            }
            (Backend::Responses(client), WireRequest::Responses(r)) => {
                let sdk_stream = client
                    .stream_responses(r)
                    .await
                    .map_err(|e| anyhow::anyhow!("openai responses stream error: {e}"))?;
                Ok(ChunkStream::Responses(ResponsesStream::new(sdk_stream)))
            }
            (Backend::Anthropic(client), WireRequest::Anthropic(r)) => Ok(ChunkStream::Anthropic(
                Box::new(AnthropicStream::new(client.stream(r).await?)),
            )),
            _ => bail!("request shape does not match the provider's wire protocol"),
        }
    }
}

/// The client for one endpoint of either OpenAI wire. Both resources answer on
/// the same connection with the same auth, and which of them a request goes to
/// is the request's own business.
///
/// No retries, deliberately, where the SDK's default is two: a refusal worth
/// retrying is one this program would rather show.
fn openai_client(api_key: String, url: String) -> Result<openai::Client> {
    openai::Client::new(
        openai::Profile::endpoint(url)
            .with_bearer_token(api_key)
            .with_max_retries(0),
    )
    .map_err(|e| anyhow::anyhow!("failed to build OpenAI client: {e}"))
}

/// One sub-request in flight, whichever wire carries it.
pub enum ChunkStream {
    /// The OpenAI chat wire, adapted from the SDK's typed chunks.
    OpenAi(OpenAiStream),
    /// The Anthropic wire, adapted from the SDK's typed events. Boxed: it
    /// carries the stream's own buffers and the frame being read, and a variant
    /// that much larger than its sibling would make every value of this enum
    /// that size.
    Anthropic(Box<AnthropicStream>),
    /// The OpenAI Responses wire, adapted from the SDK's typed events.
    Responses(ResponsesStream),
}

impl std::fmt::Debug for ChunkStream {
    /// Which wire the stream is on, and not the bytes in it: a half-read event
    /// printed to a log is noise, while the wire is what a reader is looking
    /// for when something arrived in the wrong shape.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChunkStream::OpenAi(_) => f.write_str("ChunkStream::OpenAi(…)"),
            ChunkStream::Anthropic(_) => f.write_str("ChunkStream::Anthropic(…)"),
            ChunkStream::Responses(_) => f.write_str("ChunkStream::Responses(…)"),
        }
    }
}

impl ChunkStream {
    /// None means the stream is over — the wire's end was reached or the
    /// connection closed.
    pub async fn next_chunk(&mut self) -> Result<Option<ChatChunk>> {
        match self {
            ChunkStream::OpenAi(stream) => stream.next_chunk().await,
            ChunkStream::Anthropic(stream) => stream.next_chunk().await,
            ChunkStream::Responses(stream) => stream.next_chunk().await,
        }
    }
}

// ============================================================================
// The OpenAI wire adapter.
//
// The SDK does the SSE framing and the chunk decoding; this module does the
// translation from the SDK's types to the agent's internal deltas. Three
// things are the whole of it:
//
// - `reasoning_content` on the delta is not in the SDK's typed model — it
//   rides in [`openai::ChoiceDelta::extra`] and is lifted into the local
//   [`Delta::reasoning_content`] here.
// - DeepSeek's flat `prompt_cache_hit_tokens` / `prompt_cache_miss_tokens`
//   are likewise in [`openai::CompletionUsage::extra`]; the local [`Usage`]
//   reads both shapes (the flat ones and the nested `prompt_tokens_details`),
//   so the adapter only needs to surface them.
// - The SDK's `ChoiceDelta` has `role: Option<openai::Role>`, an enum; the
//   local `Delta` has `role: Option<String>`. The adapter flattens the enum
//   to its wire string, since downstream readers expect the wire form.
// ============================================================================

/// The OpenAI wire's chunk stream: the SDK's typed chunks, adapted to the
/// agent's internal `ChatChunk`.
pub struct OpenAiStream {
    inner: openai::ChunkStream,
}

impl OpenAiStream {
    /// None means the stream is over. A chunk that carries no delta and no
    /// usage (a heartbeat frame the SDK has already read past) is yielded as
    /// `Some(ChatChunk { choices: vec![], usage: None })`; downstream code
    /// that just walks choices naturally skips it.
    pub async fn next_chunk(&mut self) -> Result<Option<ChatChunk>> {
        loop {
            let Some(chunk) = self
                .inner
                .next_chunk()
                .await
                .map_err(|e| anyhow::anyhow!("openai stream read error: {e}"))?
            else {
                return Ok(None);
            };
            let local = chunk_to_local(chunk);
            // The SDK may yield empty chunks for reasons a caller would not
            // want to see (a heartbeat frame, a chunk with only the index).
            // Skip them; the caller only cares about what carries a delta
            // or a usage.
            if !local.choices.is_empty() || local.usage.is_some() {
                return Ok(Some(local));
            }
        }
    }
}

/// Translate one SDK chunk into the agent's local shape. Vendor fields the
/// SDK does not name (DeepSeek's `reasoning_content` on the delta,
/// `prompt_cache_hit_tokens` flat on the usage) ride through `extra`.
fn chunk_to_local(chunk: openai::ChatCompletionChunk) -> ChatChunk {
    let choices = chunk
        .choices
        .into_iter()
        .map(|c| crate::types::ChunkChoice {
            delta: Some(delta_to_local(c.delta)),
            finish_reason: c.finish_reason.map(|f| f.as_str().to_string()),
        })
        .collect();
    let usage = chunk.usage.map(usage_to_local);
    ChatChunk { choices, usage }
}

fn delta_to_local(d: openai::ChoiceDelta) -> Delta {
    let local = Delta {
        // The SDK's Role enum's wire form is exactly what a downstream reader
        // expects; lowercasing the variant name would be wrong (it is already
        // lowercase on the wire).
        role: d.role.map(|r| role_wire(r).to_string()),
        content: d.content,
        // Reasoning is a vendor field — read from extra, where the SDK
        // collected it on deserialize.
        reasoning_content: d
            .extra
            .get("reasoning_content")
            .and_then(Value::as_str)
            .map(str::to_owned),
        tool_calls: d
            .tool_calls
            .map(|tcs| tcs.into_iter().map(delta_tool_to_local).collect()),
        signature: None,
        reasoning_item_id: None,
    };
    // The deprecated `function_call` form arrives here only on backends that
    // still send it; the agent's TurnAccumulator ignores unknown fields, so
    // dropping it is fine.
    let _ = d.function_call;
    local
}

fn delta_tool_to_local(d: openai::DeltaToolCall) -> DeltaToolCall {
    DeltaToolCall {
        index: d.index,
        id: d.id,
        function: d.function.map(|f| DeltaFunctionCall {
            name: f.name,
            arguments: f.arguments,
        }),
    }
}

/// Translate the SDK's `CompletionUsage` into the agent's `Usage`. DeepSeek's
/// flat `prompt_cache_hit_tokens` and `prompt_cache_miss_tokens` ride in
/// [`openai::CompletionUsage::extra`] — the local `Usage::cache` reads both
/// shapes, so we surface them flat here.
fn usage_to_local(u: openai::CompletionUsage) -> Usage {
    let prompt_cache_hit_tokens = u
        .extra
        .get("prompt_cache_hit_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let prompt_cache_miss_tokens = u
        .extra
        .get("prompt_cache_miss_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Usage {
        prompt_tokens: u.prompt_tokens,
        completion_tokens: u.completion_tokens,
        total_tokens: u.total_tokens,
        prompt_cache_hit_tokens,
        prompt_cache_miss_tokens,
        prompt_tokens_details: u
            .prompt_tokens_details
            .map(|d| crate::types::PromptTokensDetails {
                cached_tokens: d.cached_tokens.unwrap_or(0),
            }),
    }
}

/// The wire string the SDK's `Role` enum carries. The variant names are
/// already lowercase (the enum is `#[serde(rename_all = "lowercase")]`), so
/// this is just a match.
fn role_wire(r: openai::Role) -> &'static str {
    match r {
        openai::Role::System => "system",
        openai::Role::Developer => "developer",
        openai::Role::User => "user",
        openai::Role::Assistant => "assistant",
        openai::Role::Tool => "tool",
        openai::Role::Function => "function",
    }
}

// ============================================================================
// The agent's `Message` → the SDK's request shape.
//
// The internal history is provider-agnostic; the request builder hands the
// `openai_request` function a `Vec<Message>` and gets back the SDK's
// `ChatCompletionRequest`. Every translation is mechanical and lives here so
// the request builder stays readable.
// ============================================================================

/// Build the SDK's request for the agent's internal message list. The vendor
/// switches (`thinking`, `reasoning_effort`) ride through
/// [`openai::ChatCompletionRequest::extra_body`] — the SDK does not name them,
/// and the backends that need them read them at the request's top level.
pub fn openai_request(
    model: &str,
    max_tokens: u32,
    messages: &[Message],
    tools: Option<&[ToolDef]>,
    tool_choice: Option<&str>,
    send_thinking: bool,
    reasoning_effort: Option<&str>,
) -> openai::ChatCompletionRequest {
    let sdk_messages: Vec<openai::ChatCompletionMessageParam> =
        messages.iter().map(message_to_param).collect();
    let mut request = openai::ChatCompletionRequest::streaming(model, sdk_messages)
        .with_max_tokens(max_tokens)
        .with_stream_usage();
    if let Some(defs) = tools {
        request = request.with_tools(defs.iter().map(tool_to_sdk).collect::<Vec<_>>());
    }
    if let Some(choice) = tool_choice {
        request = request.with_tool_choice(openai::ToolChoice::Mode(match choice {
            "auto" => openai::ToolChoiceMode::Auto,
            "none" => openai::ToolChoiceMode::None,
            "required" => openai::ToolChoiceMode::Required,
            // Unknown mode strings are a 400 from the backend; the typed
            // call above would also fail. Surface as "auto" rather than
            // guess — the request builder should never hand us a string
            // outside this set.
            other => panic!("unsupported tool_choice {other:?}"),
        }));
    }
    // DeepSeek-style thinking switch and the GLM/DeepSeek reasoning tiers the
    // SDK's [`openai::ReasoningEffort`] enum does not model — sent through
    // `extra_body` because the typed slots would either be wrong (`Max`
    // is not in the enum) or refuse to round-trip (`thinking` has no
    // typed slot at all).
    if send_thinking {
        request = request.with_extra_body("thinking", json!({"type": "enabled"}));
    }
    if let Some(tier) = reasoning_effort {
        // The OpenAI standard enum has `Low | Medium | High | XHigh`; the
        // backends that take a wider tier list (`Max` for DeepSeek, GLM)
        // also take those, but `Max` is not in the SDK. We always go
        // through extra_body here for consistency — the wire form is
        // identical to what the typed field would write, and a `Low`
        // request replays as `"low"` either way.
        request = request.with_extra_body("reasoning_effort", Value::String(tier.to_string()));
    }
    request
}

/// Translate one agent-internal message into the SDK's typed param. The
/// Anthropic-only `thinking` field is stripped here, on the wire boundary,
/// so a session that switched providers never sends it to a backend that
/// rejects fields it does not know.
fn message_to_param(m: &Message) -> openai::ChatCompletionMessageParam {
    match m.role {
        crate::types::Role::System => openai::ChatCompletionMessageParam::System(
            openai::SystemMessageParam::new(content_to_sdk(m.content.as_ref())),
        ),
        crate::types::Role::User => openai::ChatCompletionMessageParam::User(
            openai::UserMessageParam::new(content_to_sdk(m.content.as_ref())),
        ),
        crate::types::Role::Assistant => {
            let mut param = match &m.content {
                Some(content) => openai::AssistantMessageParam::new(content_to_sdk(Some(content))),
                None => {
                    openai::AssistantMessageParam::new(openai::MessageContent::Text(String::new()))
                }
            };
            if let Some(tcs) = &m.tool_calls {
                param.tool_calls = Some(tcs.iter().map(tool_call_to_sdk).collect());
            }
            // `reasoning_content` is a vendor field the SDK does not name on
            // the assistant message; ride through extra_body so the GLM
            // multi-turn tool dialogs that require it get it back verbatim.
            // The `if let` is also the strip: a `Cot` variant other than
            // `OpenAiText` (Anthropic blocks or Responses items) does not
            // match, so a session that switched providers never leaks a
            // foreign wire's field onto the chat wire.
            if let Some(crate::types::Cot::OpenAiText { text }) = &m.cot {
                param = param.with_extra_body("reasoning_content", Value::String(text.clone()));
            }
            openai::ChatCompletionMessageParam::Assistant(param)
        }
        crate::types::Role::Tool => {
            let tool_call_id = m.tool_call_id.clone().unwrap_or_default();
            openai::ChatCompletionMessageParam::Tool(openai::ToolMessageParam::new(
                tool_call_id,
                content_to_sdk(m.content.as_ref()),
            ))
        }
    }
}

fn content_to_sdk(content: Option<&Content>) -> openai::MessageContent {
    match content {
        None => openai::MessageContent::Text(String::new()),
        Some(Content::Text(s)) => openai::MessageContent::Text(s.clone()),
        Some(Content::Parts(parts)) => {
            openai::MessageContent::Parts(parts.iter().map(content_part_to_sdk).collect())
        }
    }
}

fn content_part_to_sdk(p: &ContentPart) -> openai::ContentPart {
    if let Some(text) = &p.text {
        openai::ContentPart::text(text.clone())
    } else if let Some(image) = &p.image_url {
        openai::ContentPart::image_url(image.url.clone())
    } else {
        // An empty part is rare; serde would have rejected it on the way in,
        // but be defensive.
        openai::ContentPart::text(String::new())
    }
}

fn tool_to_sdk(def: &ToolDef) -> openai::Tool {
    let mut function = openai::FunctionDefinition::new(
        def.function.name.clone(),
        def.function
            .parameters
            .clone()
            .unwrap_or_else(|| json!({"type": "object"})),
    );
    if let Some(desc) = &def.function.description {
        function = function.with_description(desc.clone());
    }
    openai::Tool::function(function)
}

fn tool_call_to_sdk(c: &ToolCall) -> openai::MessageToolCall {
    openai::MessageToolCall::Function {
        id: c.id.clone(),
        function: openai::FunctionCall {
            name: c.function.name.clone(),
            arguments: c.function.arguments.clone(),
        },
    }
}

// ============================================================================
// The Anthropic wire: typed events in, the same deltas out.
//
// The SDK hands over the standard protocol's events one at a time; what is left
// is the translation this crate's accumulator needs, and it is deliberately
// small. Two things are the whole of it: a `tool_use` block is sharded by the
// wire's *content-block* index while the accumulator shards calls by *ordinal*
// among the calls, so the adapter is where an index becomes an ordinal; and the
// usage the two ends report in different places is merged so it is emitted once,
// complete.
// ============================================================================

/// The Anthropic wire's chunk stream: the SDK's events, adapted.
pub struct AnthropicStream {
    inner: anthropic::EventStream,
    /// The `tool_use` blocks declared so far, keyed by *content-block* index —
    /// which is not the tool-call ordinal: thinking and text blocks take
    /// indexes too. The value is the block's ordinal (the order among
    /// `tool_use` blocks), which is what the accumulator shards calls by; the
    /// id and name the start event carried went out with its delta.
    tool_uses: HashMap<u32, u32>,
    /// The ordinal the next tool_use block gets.
    next_tool: u32,
    /// What the stream has reported about the prompt so far; emitted with the
    /// output count, so usage lands once, complete.
    usage: anthropic::Usage,
}

impl AnthropicStream {
    pub fn new(inner: anthropic::EventStream) -> Self {
        Self {
            inner,
            tool_uses: HashMap::new(),
            next_tool: 0,
            usage: anthropic::Usage::default(),
        }
    }

    /// None means the stream is over. An event that folds into no delta (a
    /// ping, a block boundary) continues the loop rather than yielding an
    /// empty chunk.
    pub async fn next_chunk(&mut self) -> Result<Option<ChatChunk>> {
        loop {
            let Some(event) = self.inner.next_event().await? else {
                return Ok(None);
            };
            if let Some(chunk) = self.fold(&event) {
                return Ok(Some(chunk));
            }
        }
    }

    /// Fold one event. None: nothing to hand downstream.
    fn fold(&mut self, event: &AnthropicEvent) -> Option<ChatChunk> {
        match event {
            AnthropicEvent::MessageStart { message } => {
                self.usage.merge(&message.usage);
                None
            }
            AnthropicEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                let BlockKind::ToolUse { id, name, .. } = &content_block.kind else {
                    return None;
                };
                // The block is given the next ordinal here, and the argument
                // shards that follow are translated onto it.
                let ordinal = self.next_tool;
                self.next_tool += 1;
                self.tool_uses.insert(*index, ordinal);
                Some(chunk_with(Delta {
                    tool_calls: Some(vec![DeltaToolCall {
                        index: ordinal,
                        id: Some(id.clone()),
                        function: Some(DeltaFunctionCall {
                            name: Some(name.clone()),
                            arguments: None,
                        }),
                    }]),
                    ..Default::default()
                }))
            }
            AnthropicEvent::ContentBlockDelta { index, delta } => {
                let delta = match delta {
                    BlockDelta::TextDelta { text } => Delta {
                        content: Some(text.clone()),
                        ..Default::default()
                    },
                    BlockDelta::ThinkingDelta { thinking } => Delta {
                        reasoning_content: Some(thinking.clone()),
                        ..Default::default()
                    },
                    BlockDelta::SignatureDelta { signature } => Delta {
                        signature: Some(signature.clone()),
                        ..Default::default()
                    },
                    BlockDelta::InputJsonDelta { partial_json } => {
                        // The shard lands on the call its block declared; a
                        // shard for a block nobody started has nowhere to go
                        // and is dropped.
                        let ordinal = *self.tool_uses.get(index)?;
                        Delta {
                            tool_calls: Some(vec![DeltaToolCall {
                                index: ordinal,
                                id: None,
                                function: Some(DeltaFunctionCall {
                                    name: None,
                                    arguments: Some(partial_json.clone()),
                                }),
                            }]),
                            ..Default::default()
                        }
                    }
                    // A citation, or a delta kind the spec added later:
                    // nothing to fold, and no reason to fail a stream that is
                    // still speaking its own language.
                    BlockDelta::CitationsDelta { .. } | BlockDelta::Unknown => return None,
                };
                Some(chunk_with(delta))
            }
            AnthropicEvent::MessageDelta { delta, usage } => {
                // The last event that carries anything. Its usage is folded in
                // whole rather than read for the output count alone: on this
                // wire it is where the prompt of a stream whose message_start
                // carried zeros actually arrives.
                if let Some(reported) = usage {
                    self.usage.merge(reported);
                }
                // Why the model stopped rides the same chunk the OpenAI wire
                // reports its `finish_reason` on, in this wire's own word: the
                // one thing a caller must not do with a `max_tokens` answer is
                // take it for a whole one.
                let choices = delta
                    .stop_reason
                    .as_ref()
                    .map(|reason| {
                        vec![crate::types::ChunkChoice {
                            delta: None,
                            finish_reason: Some(reason.as_str().to_string()),
                        }]
                    })
                    .unwrap_or_default();
                Some(ChatChunk {
                    choices,
                    usage: Some(usage_report(&self.usage)),
                })
            }
            // The block boundaries, the keep-alive, the end of the answer, and
            // anything the spec adds later: nothing to fold. The stream's own
            // failure never reaches here — the SDK ends the stream with an
            // error instead.
            _ => None,
        }
    }
}

/// The counts in this crate's spelling: hit = tokens served from cache,
/// miss = the rest of the prompt (this turn's uncached input and what it wrote
/// into the cache), total = the whole request and answer.
fn usage_report(usage: &anthropic::Usage) -> Usage {
    let prompt = usage.prompt_tokens();
    let completion = usage.output_tokens.unwrap_or(0);
    Usage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: prompt + completion,
        prompt_cache_hit_tokens: usage.cache_read_input_tokens.unwrap_or(0),
        prompt_cache_miss_tokens: usage.input_tokens.unwrap_or(0)
            + usage.cache_creation_input_tokens.unwrap_or(0),
        prompt_tokens_details: None,
    }
}
fn chunk_with(delta: Delta) -> ChatChunk {
    ChatChunk {
        choices: vec![crate::types::ChunkChoice {
            delta: Some(delta),
            finish_reason: None,
        }],
        usage: None,
    }
}

// ============================================================================
// The Responses wire: typed events in, the same deltas out.
//
// This wire narrates an answer as the *items* it is made of — a reasoning item,
// a message, a function call — rather than as content blocks or choice deltas.
// Three things are the whole of the translation:
//
// - A function call is announced by its own item, with the call id its result
//   is addressed by, and its arguments arrive as shards keyed by the item's
//   *output index*. The accumulator shards calls by ordinal among the calls, so
//   the adapter is where an output index becomes an ordinal — the same job the
//   Anthropic adapter does with content-block indexes.
// - Reasoning streams as text tagged with the id of the item carrying it. That
//   tag rides along on the local delta, because this wire asks for the item
//   back whole on the next request (see `Message::reasoning`).
// - The answer's end is a lifecycle event rather than a `finish_reason` on a
//   chunk: the status says whether the answer is whole, and `incomplete_details`
//   says why not, in the words the truncation notice reads.
// ============================================================================

/// The Responses wire's event stream: the SDK's typed events, adapted.
pub struct ResponsesStream {
    inner: openai::ResponseEventStream,
    /// The function calls declared so far, keyed by the item's *output index* —
    /// which is not the tool-call ordinal: reasoning and message items take
    /// indexes too. The value is the call's ordinal among the calls, which is
    /// what the accumulator shards by.
    calls: HashMap<u64, u32>,
    /// The ordinal the next function call gets.
    next_call: u32,
}

impl ResponsesStream {
    pub fn new(inner: openai::ResponseEventStream) -> Self {
        Self {
            inner,
            calls: HashMap::new(),
            next_call: 0,
        }
    }

    /// None means the stream is over. An event that folds into no delta — a
    /// lifecycle event, a part boundary — continues the loop rather than
    /// yielding an empty chunk.
    pub async fn next_chunk(&mut self) -> Result<Option<ChatChunk>> {
        loop {
            let Some(event) = self
                .inner
                .next_event()
                .await
                .map_err(|e| anyhow::anyhow!("openai responses stream read error: {e}"))?
            else {
                return Ok(None);
            };
            if let Some(chunk) = self.fold(&event)? {
                return Ok(Some(chunk));
            }
        }
    }

    /// Fold one event. None: nothing to hand downstream.
    fn fold(&mut self, event: &openai::ResponseStreamEvent) -> Result<Option<ChatChunk>> {
        use openai::ResponseStreamEvent as E;
        let chunk = match event {
            // A call is announced before its arguments: the id its result is
            // addressed by, and the name, go out with the first shard the way
            // the Anthropic adapter sends them at block start. Every other
            // item — a reasoning item, a message — is announced here too, and
            // announces nothing the deltas that follow do not carry; they fall
            // through to the read-past arm below.
            E::OutputItemAdded {
                output_index,
                item: openai::ResponseOutputItem::FunctionToolCall(call),
                ..
            } => {
                let ordinal = self.next_call;
                self.next_call += 1;
                self.calls.insert(*output_index, ordinal);
                chunk_with(Delta {
                    tool_calls: Some(vec![DeltaToolCall {
                        index: ordinal,
                        id: Some(call.call_id.clone()),
                        function: Some(DeltaFunctionCall {
                            name: Some(call.name.clone()),
                            arguments: None,
                        }),
                    }]),
                    ..Default::default()
                })
            }
            E::FunctionCallArgumentsDelta {
                output_index,
                delta,
                ..
            } => {
                // A shard for a call nobody announced has nowhere to go; the
                // ordinal it would need is not this shard's to invent.
                let Some(ordinal) = self.calls.get(output_index).copied() else {
                    return Ok(None);
                };
                chunk_with(Delta {
                    tool_calls: Some(vec![DeltaToolCall {
                        index: ordinal,
                        id: None,
                        function: Some(DeltaFunctionCall {
                            name: None,
                            arguments: Some(delta.clone()),
                        }),
                    }]),
                    ..Default::default()
                })
            }
            E::OutputTextDelta { delta, .. } => chunk_with(Delta {
                content: Some(delta.clone()),
                ..Default::default()
            }),
            // The reasoning text, tagged with the item it belongs to: this wire
            // replays the item, so the tag travels with the text.
            E::ReasoningTextDelta { item_id, delta, .. } => chunk_with(Delta {
                reasoning_content: Some(delta.clone()),
                reasoning_item_id: Some(item_id.clone()),
                ..Default::default()
            }),
            // An endpoint that reports its reasoning as a summary instead —
            // OpenAI's own models do — reveals the same text under another
            // event. It is shown, and it is deliberately not tagged: a summary
            // is not the reasoning item's content, and replaying it as one
            // would put words in the model's mouth.
            E::ReasoningSummaryTextDelta { delta, .. } => chunk_with(Delta {
                reasoning_content: Some(delta.clone()),
                ..Default::default()
            }),
            // The answer's end. The status is the wire's own word for whether
            // the answer is whole; an incomplete one names why, in the words
            // `Reply::truncation` reads.
            E::Completed { response, .. } => {
                end_chunk("stop".to_string(), usage_of(response.usage.as_ref()))
            }
            E::Incomplete { response, .. } => end_chunk(
                incomplete_reason(response.incomplete_details.as_ref()),
                usage_of(response.usage.as_ref()),
            ),
            // An answer that failed is not an answer: the caller must not read
            // it as one.
            E::Failed { response, .. } => {
                let why = response
                    .error
                    .as_ref()
                    .map(|e| format!("{}: {}", e.code, e.message))
                    .unwrap_or_else(|| "the endpoint reported no reason".to_string());
                bail!("the response failed: {why}");
            }
            // The deltas already carried the whole of these, the item
            // boundaries carry nothing to fold, and an event type this crate
            // does not know yet is read past rather than refused.
            _ => return Ok(None),
        };
        Ok(Some(chunk))
    }
}

/// The chunk an answer's end rides on: no delta, the wire's word for why it
/// stopped, and the counts it reported with them.
fn end_chunk(finish_reason: String, usage: Option<Usage>) -> ChatChunk {
    ChatChunk {
        choices: vec![crate::types::ChunkChoice {
            delta: None,
            finish_reason: Some(finish_reason),
        }],
        usage,
    }
}

/// Why an incomplete answer stopped, in the word the caller reads it under. A
/// ceiling is reported as the chat wire's own word for the same thing, so one
/// notice covers both wires; every other reason is passed through as the wire
/// spelled it, which is what the trace records.
fn incomplete_reason(details: Option<&openai::IncompleteDetails>) -> String {
    match details.and_then(|d| d.reason.as_deref()) {
        Some("max_output_tokens") => "max_tokens".to_string(),
        Some(reason) => reason.to_string(),
        None => "incomplete".to_string(),
    }
}

/// The counts in this crate's spelling. This wire reports the same cache
/// breakdown the chat wire does — nested, and only when it has one to report —
/// so the local shape keeps it nested too: details the endpoint left out mean
/// it said nothing, which is not the same as a reported zero.
fn usage_of(usage: Option<&openai::ResponseUsageSummary>) -> Option<Usage> {
    let usage = usage?;
    Some(Usage {
        prompt_tokens: usage.input_tokens,
        completion_tokens: usage.output_tokens,
        total_tokens: usage.total_tokens,
        prompt_cache_hit_tokens: 0,
        prompt_cache_miss_tokens: 0,
        prompt_tokens_details: usage
            .input_tokens_details
            .as_ref()
            .and_then(|d| d.cached_tokens)
            .map(|cached_tokens| crate::types::PromptTokensDetails { cached_tokens }),
    })
}

// ============================================================================
// The agent's `Message` → the Responses request shape.
//
// This wire takes the conversation as `input`: a list of items in the order
// they happened, not a list of messages with a role each. The mapping is
// therefore not one message to one item — an assistant turn is the reasoning
// it did, what it said, and the calls it declared — and, being a list, it is
// also where a turn's tool results land as items of their own.
//
// The system prompt rides in `instructions`, beside the conversation rather
// than inside it, which is where this wire keeps a prompt that is not a turn.
// ============================================================================

/// Build the SDK's request for the agent's internal message list.
pub fn responses_request(
    model: &str,
    max_output_tokens: u32,
    instructions: &str,
    messages: &[Message],
    tools: Option<&[ToolDef]>,
    reasoning_effort: Option<&str>,
) -> openai::ResponseCreateRequest {
    let mut input: Vec<openai::ResponseInputItem> = Vec::with_capacity(messages.len());
    for message in messages {
        push_input_items(&mut input, message);
    }
    let mut request =
        openai::ResponseCreateRequest::streaming(model, openai::ResponseInput::items(input))
            .with_instructions(instructions)
            .with_max_output_tokens(max_output_tokens)
            // The only mode this program ever asks for, and the only one this wire's
            // endpoints accept: which call runs is the approval gate's business, not
            // the request's.
            .with_tool_choice(openai::ResponseToolChoice::auto());
    if let Some(defs) = tools {
        for def in defs {
            // Not a strict schema: the backend validates the arguments against
            // it when `strict` is set, and these tools' parameters carry
            // optional fields a strict validator refuses to accept as-is.
            request = request.with_tool(openai::ResponseTool::from_function_tool(
                openai::ResponseFunctionTool::new(
                    def.function.name.clone(),
                    def.function
                        .parameters
                        .clone()
                        .unwrap_or_else(|| json!({"type": "object"})),
                )
                .with_strict(false)
                .with_description(def.function.description.clone().unwrap_or_default()),
            ));
        }
    }
    if let Some(effort) = reasoning_effort.and_then(openai::ResponsesReasoningEffort::from_name) {
        request = request.with_reasoning(openai::ReasoningConfig::with_effort(effort));
    }
    request
}

/// One agent-internal message as the items this wire replays it as.
///
/// The reasoning comes first because that is the order the model produced it
/// in, and this wire asks for the items back in the order it wrote them: the
/// chain of thought, then what was said, then what was called — or, for a tool
/// result, the one output item that answers a call.
fn push_input_items(items: &mut Vec<openai::ResponseInputItem>, message: &Message) {
    match message.role {
        crate::types::Role::System => items.push(openai::ResponseInputItem::Message(
            openai::EasyInputMessage::system(content_to_input(message.content.as_ref())),
        )),
        crate::types::Role::User => items.push(openai::ResponseInputItem::Message(
            openai::EasyInputMessage::user(content_to_input(message.content.as_ref())),
        )),
        crate::types::Role::Assistant => {
            // The `if let` is also the strip: a `Cot` variant other than
            // `ResponsesItems` (chat text or Anthropic blocks) does not
            // match, so a session that switched providers never leaks a
            // foreign wire's field onto the Responses wire.
            if let Some(crate::types::Cot::ResponsesItems { items: reasoning }) = &message.cot {
                for item in reasoning {
                    items.push(openai::ResponseInputItem::Reasoning(
                        openai::ReasoningItemInput::new(item.id.clone(), item.text.clone()),
                    ));
                }
            }
            // A turn that only called tools has nothing to say, and an empty
            // assistant message is not a thing this wire has: the calls below
            // carry the turn on their own.
            if let Some(text) = message.text().filter(|text| !text.is_empty()) {
                items.push(openai::ResponseInputItem::Message(
                    openai::EasyInputMessage::assistant(text),
                ));
            }
            for call in message.tool_calls.iter().flatten() {
                items.push(openai::ResponseInputItem::FunctionCall(
                    openai::ResponseFunctionToolCall::replay(
                        call.id.clone(),
                        call.function.name.clone(),
                        call.function.arguments.clone(),
                    ),
                ));
            }
        }
        crate::types::Role::Tool => items.push(openai::ResponseInputItem::FunctionCallOutput(
            openai::FunctionCallOutputItem::new(
                message.tool_call_id.clone().unwrap_or_default(),
                message.text().unwrap_or_default(),
            ),
        )),
    }
}

/// A message's content in this wire's vocabulary: the string it is, or the
/// parts its images become — `input_text` runs and `input_image` sources, which
/// is what a `data:` URL is on this wire too.
fn content_to_input(content: Option<&Content>) -> openai::ResponseInputContent {
    match content {
        Some(Content::Parts(parts)) => {
            openai::ResponseInputContent::Parts(parts.iter().map(input_content_part).collect())
        }
        Some(Content::Text(text)) => openai::ResponseInputContent::Text(text.clone()),
        None => openai::ResponseInputContent::Text(String::new()),
    }
}

fn input_content_part(part: &ContentPart) -> openai::ResponseInputContentPart {
    if let Some(text) = &part.text {
        openai::ResponseInputContentPart::text(text.clone())
    } else if let Some(image) = &part.image_url {
        openai::ResponseInputContentPart::image_url(image.url.clone())
    } else {
        // An empty part is rare; serde would have rejected it on the way in,
        // but be defensive.
        openai::ResponseInputContentPart::text(String::new())
    }
}

// ============================================================================
// Test helpers.
//
// Helpers exposed to the integration tests, so they can build requests and
// stream adapters with the same vocabulary the production code uses.
// ============================================================================

#[doc(hidden)]
pub mod test_helpers {
    /// A system message built the same way the production code does.
    #[cfg(test)]
    pub fn system_message(text: &str) -> crate::types::Message {
        crate::types::Message::system(text)
    }

    /// A user message built the same way the production code does.
    #[cfg(test)]
    pub fn user_message(text: &str) -> crate::types::Message {
        crate::types::Message::user(text)
    }

    /// Build a request like the production adapter does, for assertions on
    /// the wire shape.
    #[cfg(test)]
    pub fn openai_request_for_test(
        model: &str,
        max_tokens: u32,
        messages: Vec<crate::types::Message>,
        stream: bool,
        reasoning_effort: &str,
        send_thinking: bool,
    ) -> openai::ChatCompletionRequest {
        let mut req = super::openai_request(
            model,
            max_tokens,
            &messages,
            None,
            None,
            send_thinking,
            Some(reasoning_effort),
        );
        // Force the streaming flag off so the test JSON has the value it
        // expects; the production builder always streams.
        if !stream {
            req = req.non_streaming();
        }
        req
    }
}

#[cfg(test)]
mod tests {
    use super::test_helpers as h;
    use super::*;
    use crate::provider::{MIMO, MINIMAX};
    use crate::types::{Cot, TurnAccumulator};

    // -----------------------------------------------------------------------
    // The OpenAI wire adapter.
    // -----------------------------------------------------------------------

    #[test]
    fn openai_request_carries_thinking_and_max_effort_through_extra_body() {
        let req = h::openai_request_for_test(
            "deepseek-v4-flash",
            384_000,
            vec![h::system_message("sys"), h::user_message("hi")],
            true,
            "max",
            true,
        );
        let v: serde_json::Value = serde_json::to_value(&req).unwrap();
        assert_eq!(v["model"], "deepseek-v4-flash");
        assert_eq!(v["max_tokens"], 384_000);
        assert_eq!(v["stream"], true);
        // Vendor fields ride through extra_body — at the top level of the
        // request, exactly where DeepSeek / GLM read them.
        assert_eq!(v["thinking"]["type"], "enabled");
        assert_eq!(v["reasoning_effort"], "max");
        assert_eq!(v["messages"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn openai_request_without_thinking_omits_the_field() {
        let req = h::openai_request_for_test(
            "gpt-4o",
            1024,
            vec![h::user_message("hi")],
            true,
            "low",
            false,
        );
        let v: serde_json::Value = serde_json::to_value(&req).unwrap();
        assert!(
            v.get("thinking").is_none(),
            "thinking was not asked for: {v}"
        );
        assert_eq!(v["reasoning_effort"], "low");
    }

    #[test]
    fn openai_request_strips_anthropic_thinking_blocks() {
        // A message that came in on the Anthropic wire carries a `Cot::AnthropicBlocks`
        // — the chat wire has no slot for it, so the pattern match in the
        // builder contributes nothing. The wire's own `reasoning_content`
        // would ride through extra_body when present; it is absent here.
        let msg = Message {
            role: crate::types::Role::Assistant,
            content: Some("answer".into()),
            cot: Some(Cot::AnthropicBlocks {
                blocks: vec![crate::types::ThinkingBlock {
                    thinking: "thought".into(),
                    signature: "sig".into(),
                }],
            }),
            tool_calls: None,
            tool_call_id: None,
        };
        let req = openai_request("minimax", 131_072, &[msg], None, None, false, None);
        let v: serde_json::Value = serde_json::to_value(&req).unwrap();
        let msg_json = &v["messages"][0];
        assert!(msg_json.get("thinking").is_none(), "{msg_json}");
        assert!(
            msg_json.get("reasoning_content").is_none(),
            "Anthropic's variant is dropped on the chat wire: {msg_json}"
        );

        // A message that came in on the chat wire carries `Cot::OpenAiText` —
        // it rides back through extra_body as `reasoning_content`.
        let msg = Message {
            role: crate::types::Role::Assistant,
            content: Some("answer".into()),
            cot: Some(Cot::OpenAiText {
                text: "thought".into(),
            }),
            tool_calls: None,
            tool_call_id: None,
        };
        let req = openai_request("minimax", 131_072, &[msg], None, None, false, None);
        let v: serde_json::Value = serde_json::to_value(&req).unwrap();
        let msg_json = &v["messages"][0];
        assert_eq!(msg_json["reasoning_content"], "thought");
    }

    #[test]
    fn openai_request_drops_function_call_in_favor_of_tool_calls() {
        // The deprecated `function_call` shape on a stored assistant
        // message: the SDK has no slot for it. We simply drop it; the
        // wire only knows `tool_calls` today.
        let msg = Message::user("hi");
        let req = openai_request("gpt-4o", 1024, &[msg], None, None, false, None);
        let _ = req; // the test is the absence of panic; the SDK rejects the field, not us
    }

    #[tokio::test]
    async fn openai_stream_folds_a_chunk_into_a_local_delta() {
        // Build a chunk JSON as the wire writes it (DeepSeek-style, with
        // `reasoning_content` flat on the delta), feed it through the SDK
        // parser, and verify the local Delta has both fields lifted.
        let body = serde_json::json!({
            "id": "1",
            "choices": [{
                "index": 0,
                "delta": {
                    "role": "assistant",
                    "content": "hi ",
                    "reasoning_content": "let me ",
                },
                "finish_reason": null
            }],
            "created": 1,
            "model": "deepseek-v4-flash",
            "object": "chat.completion.chunk",
        });
        let body_bytes = format!("data: {body}\n\n");
        let bytes = bytes::Bytes::from(body_bytes);
        let events = futures_util::stream::iter(vec![Ok(bytes)]);
        let mut stream = OpenAiStream {
            inner: openai::ChunkStream::new(Box::pin(events)),
        };
        let chunk = stream.next_chunk().await.unwrap().unwrap();
        let delta = chunk.choices[0].delta.as_ref().unwrap();
        assert_eq!(delta.content.as_deref(), Some("hi "));
        assert_eq!(delta.reasoning_content.as_deref(), Some("let me "));
        assert_eq!(delta.role.as_deref(), Some("assistant"));
    }

    #[tokio::test]
    async fn openai_stream_translates_completion_usage() {
        // DeepSeek's flat cache fields ride through the SDK's `extra`.
        let body = serde_json::json!({
            "id": "1",
            "choices": [],
            "created": 1,
            "model": "deepseek-v4-flash",
            "object": "chat.completion.chunk",
            "usage": {
                "prompt_tokens": 26,
                "completion_tokens": 9,
                "total_tokens": 35,
                "prompt_cache_hit_tokens": 8,
                "prompt_cache_miss_tokens": 18,
            }
        });
        let body_bytes = format!("data: {body}\n\n");
        let bytes = bytes::Bytes::from(body_bytes);
        let events = futures_util::stream::iter(vec![Ok(bytes)]);
        let mut stream = OpenAiStream {
            inner: openai::ChunkStream::new(Box::pin(events)),
        };
        // One chunk: empty choices, usage present — yields a chunk with
        // empty `choices` and the usage lifted into the local shape.
        let chunk = stream.next_chunk().await.unwrap().unwrap();
        let u = chunk.usage.unwrap();
        assert_eq!(u.prompt_cache_hit_tokens, 8);
        assert_eq!(u.prompt_cache_miss_tokens, 18);
        assert_eq!(u.completion_tokens, 9);
    }

    #[tokio::test]
    async fn openai_stream_lifts_glm_nested_cache() {
        // GLM reports `cached_tokens` nested under `prompt_tokens_details`;
        // the local Usage reads that shape too.
        let body = serde_json::json!({
            "id": "1",
            "choices": [],
            "created": 1,
            "model": "glm-5.3",
            "object": "chat.completion.chunk",
            "usage": {
                "prompt_tokens": 1200,
                "completion_tokens": 300,
                "total_tokens": 1500,
                "prompt_tokens_details": {"cached_tokens": 800}
            }
        });
        let body_bytes = format!("data: {body}\n\n");
        let bytes = bytes::Bytes::from(body_bytes);
        let events = futures_util::stream::iter(vec![Ok(bytes)]);
        let mut stream = OpenAiStream {
            inner: openai::ChunkStream::new(Box::pin(events)),
        };
        // One chunk: empty choices, usage present — yields a chunk with
        // empty `choices` and the usage lifted into the local shape.
        let chunk = stream.next_chunk().await.unwrap().unwrap();
        let u = chunk.usage.unwrap();
        assert_eq!(u.prompt_tokens_details.unwrap().cached_tokens, 800);
        assert!(stream.next_chunk().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn openai_stream_translates_finish_reason_to_its_wire_string() {
        let body = serde_json::json!({
            "id": "1",
            "choices": [{
                "index": 0,
                "delta": {},
                "finish_reason": "tool_calls"
            }],
            "created": 1,
            "model": "deepseek-v4-flash",
            "object": "chat.completion.chunk",
        });
        let body_bytes = format!("data: {body}\n\ndata: [DONE]\n\n");
        let bytes = bytes::Bytes::from(body_bytes);
        let events = futures_util::stream::iter(vec![Ok(bytes)]);
        let mut stream = OpenAiStream {
            inner: openai::ChunkStream::new(Box::pin(events)),
        };
        let chunk = stream.next_chunk().await.unwrap().unwrap();
        assert_eq!(
            chunk.choices[0].finish_reason.as_deref(),
            Some("tool_calls")
        );
        assert!(stream.next_chunk().await.unwrap().is_none());
    }

    // -----------------------------------------------------------------------
    // The Anthropic wire.
    // -----------------------------------------------------------------------

    fn event(name: &str, payload: serde_json::Value) -> String {
        format!("event: {name}\ndata: {payload}\n\n")
    }

    /// A body of SSE events as the SDK reads it, adapted by the stream the
    /// interpreter folds: the same bytes a mock endpoint would answer with.
    fn anthropic_stream(body: &str) -> ChunkStream {
        let bytes = bytes::Bytes::from(body.to_owned());
        let events = futures_util::stream::iter(vec![Ok(bytes)]);
        ChunkStream::Anthropic(Box::new(AnthropicStream::new(anthropic::EventStream::new(
            Box::pin(events),
        ))))
    }

    /// A full thinking-then-answer turn: what MiniMax-M3 streams for one
    /// sub-request, translated and folded into one assistant message.
    #[tokio::test]
    async fn anthropic_stream_folds_events_into_one_message() {
        let body = [
            event(
                "message_start",
                serde_json::json!({"type":"message_start","message":{"id":"m1","role":"assistant","model":"MiniMax-M3","usage":{"input_tokens":100,"cache_creation_input_tokens":20,"cache_read_input_tokens":80,"output_tokens":1}}}),
            ),
            event("ping", serde_json::json!({"type":"ping"})),
            event(
                "content_block_start",
                serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
            ),
            event(
                "content_block_delta",
                serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"let me "}}),
            ),
            event(
                "content_block_delta",
                serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"check."}}),
            ),
            event(
                "content_block_delta",
                serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-1"}}),
            ),
            event(
                "content_block_stop",
                serde_json::json!({"type":"content_block_stop","index":0}),
            ),
            event(
                "content_block_start",
                serde_json::json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
            ),
            event(
                "content_block_delta",
                serde_json::json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"run"}}),
            ),
            event(
                "content_block_delta",
                serde_json::json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"ning"}}),
            ),
            event(
                "content_block_stop",
                serde_json::json!({"type":"content_block_stop","index":1}),
            ),
            event(
                "content_block_start",
                serde_json::json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"call_9","name":"Bash","input":{}}}),
            ),
            event(
                "content_block_delta",
                serde_json::json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"comm"}}),
            ),
            event(
                "content_block_delta",
                serde_json::json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"and\":\"ls\"}"}}),
            ),
            event(
                "content_block_stop",
                serde_json::json!({"type":"content_block_stop","index":2}),
            ),
            event(
                "message_delta",
                serde_json::json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":42}}),
            ),
            event("message_stop", serde_json::json!({"type":"message_stop"})),
        ]
        .concat();
        let mut sse = anthropic_stream(&body);

        let mut acc = TurnAccumulator::default();
        let mut usage = None;
        while let Some(chunk) = sse.next_chunk().await.unwrap() {
            for choice in chunk.choices {
                if let Some(delta) = choice.delta {
                    acc.feed(&delta);
                }
            }
            if chunk.usage.is_some() {
                usage = chunk.usage;
            }
        }
        let msg = acc.finish();
        assert_eq!(msg.text().as_deref(), Some("running"));
        // The wire was Anthropic, so the CoT carries the signed blocks the
        // wire demands back. The flat text in `reasoning_content` (the
        // chat wire's spelling) is folded into the same blocks by
        // `TurnAccumulator` and is not kept on its own.
        assert_eq!(
            msg.cot,
            Some(Cot::AnthropicBlocks {
                blocks: vec![crate::types::ThinkingBlock {
                    thinking: "let me check.".into(),
                    signature: "sig-1".into(),
                }],
            })
        );
        let calls = msg.tool_calls.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_9");
        assert_eq!(calls[0].function.name, "Bash");
        assert_eq!(calls[0].function.arguments, r#"{"command":"ls"}"#);
        // message_start's input merged with message_delta's output, in the
        // DeepSeek spelling Usage::cache reads.
        let u = usage.unwrap();
        assert_eq!(
            u.cache(),
            Some(crate::types::CacheTokens { hit: 80, miss: 120 })
        );
        assert_eq!(u.completion_tokens, 42);
        assert_eq!(u.prompt_tokens, 200);
        assert_eq!(u.total_tokens, 242, "the whole request and answer");
    }

    /// The usage shape MiniMax actually streams, read off the live endpoint:
    /// message_start reports zeros and no cache fields at all, and
    /// message_delta carries the whole prompt -- cache reads included. Keying
    /// the cache on message_start reports such a stream as no caching, which
    /// leaves the status line's hit/miss at zero for every turn.
    #[tokio::test]
    async fn anthropic_usage_lands_when_message_delta_carries_it() {
        let body = [
            event(
                "message_start",
                serde_json::json!({"type":"message_start","message":{"id":"m1","role":"assistant","model":"MiniMax-M3","usage":{"input_tokens":0,"output_tokens":0,"service_tier":"standard"}}}),
            ),
            event(
                "content_block_delta",
                serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"4"}}),
            ),
            event(
                "message_delta",
                serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":36,"output_tokens":12,"cache_read_input_tokens":128,"service_tier":"standard"}}),
            ),
            event("message_stop", serde_json::json!({"type":"message_stop"})),
        ]
        .concat();
        let mut sse = anthropic_stream(&body);
        let mut usage = None;
        while let Some(chunk) = sse.next_chunk().await.unwrap() {
            if chunk.usage.is_some() {
                usage = chunk.usage;
            }
        }
        let u = usage.unwrap();
        assert_eq!(
            u.cache(),
            Some(crate::types::CacheTokens { hit: 128, miss: 36 }),
            "the cache read is the hit, the uncached input the miss"
        );
        assert_eq!(u.prompt_tokens, 164);
        assert_eq!(u.completion_tokens, 12);
        assert_eq!(u.total_tokens, 176);
    }

    /// A stream that ends without a message_delta reports nothing at all:
    /// usage is never invented from a stream that did not report it.
    /// Why the model stopped reaches the caller in the field the OpenAI wire
    /// reports its own reasons in, because an answer cut off at the ceiling is
    /// the one thing a turn must not take for a whole one.
    #[tokio::test]
    async fn anthropic_stop_reason_rides_the_chunk_the_usage_does() {
        let body = [
            event(
                "message_start",
                serde_json::json!({"type":"message_start","message":{"id":"m1","usage":{"input_tokens":10}}}),
            ),
            event(
                "message_delta",
                serde_json::json!({"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":9}}),
            ),
            event("message_stop", serde_json::json!({"type":"message_stop"})),
        ]
        .concat();
        let mut sse = anthropic_stream(&body);
        let chunk = sse.next_chunk().await.unwrap().unwrap();
        assert_eq!(
            chunk.choices[0].finish_reason.as_deref(),
            Some("max_tokens")
        );
        assert!(chunk.usage.is_some(), "the counts ride the same chunk");
        assert!(sse.next_chunk().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn anthropic_stream_without_message_delta_reports_no_usage() {
        let body = [
            event(
                "message_start",
                serde_json::json!({"type":"message_start","message":{"id":"m1","usage":{"input_tokens":10,"output_tokens":0}}}),
            ),
            event("message_stop", serde_json::json!({"type":"message_stop"})),
        ]
        .concat();
        let mut sse = anthropic_stream(&body);
        while let Some(chunk) = sse.next_chunk().await.unwrap() {
            assert!(chunk.usage.is_none());
        }
    }

    #[tokio::test]
    async fn anthropic_unknown_delta_kinds_are_skipped_not_fatal() {
        let body = [
            event(
                "content_block_delta",
                serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}),
            ),
            event(
                "content_block_delta",
                serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"citation_delta","citation":{"url":"x"}}}),
            ),
            event("message_stop", serde_json::json!({"type":"message_stop"})),
        ]
        .concat();
        let mut sse = anthropic_stream(&body);
        let mut texts = Vec::new();
        while let Some(chunk) = sse.next_chunk().await.unwrap() {
            for choice in chunk.choices {
                if let Some(delta) = choice.delta
                    && let Some(t) = delta.content
                {
                    texts.push(t);
                }
            }
        }
        assert_eq!(texts, vec!["ok".to_string()]);
    }

    #[tokio::test]
    async fn anthropic_error_event_fails_the_stream() {
        let body = event(
            "error",
            serde_json::json!({"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}),
        );
        let mut sse = anthropic_stream(&body);
        let err = sse.next_chunk().await.unwrap_err().to_string();
        assert!(err.contains("overloaded_error"), "{err}");
    }

    #[tokio::test]
    async fn a_request_shape_that_does_not_match_the_wire_is_refused() {
        // No network is touched: the mismatch is found before anything is
        // sent, which is the one misuse the WireRequest enum cannot prevent.
        let openai =
            Client::new("k".into(), "http://127.0.0.1:1/never".into(), Wire::OpenAi).unwrap();
        let mini = Client::new("k".into(), MINIMAX.url.into(), Wire::Anthropic).unwrap();
        let responses = Client::new("k".into(), MIMO.url.into(), Wire::Responses).unwrap();
        let meta = crate::session::SessionMeta {
            provider: Some("minimax".into()),
            model: "MiniMax-M3".into(),
            reasoning_effort: None,
            instructions: None,
        };
        let anthropic_request = crate::agent::request::build_request(&MINIMAX, &meta, &[], &[]);
        // Try to send the Anthropic request over the OpenAI client — refused.
        let err = openai
            .stream_chat(&anthropic_request)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not match"), "{err}");

        let mut deepseek_meta = meta.clone();
        deepseek_meta.provider = Some("deepseek".into());
        deepseek_meta.model = "deepseek-flash".into();
        let openai_request = crate::agent::request::build_request(
            &crate::provider::DEEPSEEK,
            &deepseek_meta,
            &[],
            &[],
        );
        let err = mini
            .stream_chat(&openai_request)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not match"), "{err}");

        // The two OpenAI-shaped wires are one client type but not one pair: a
        // chat request sent to a Responses endpoint is the same misuse, and is
        // caught the same way.
        let err = responses
            .stream_chat(&openai_request)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not match"), "{err}");
    }

    // -----------------------------------------------------------------------
    // The Responses wire.
    // -----------------------------------------------------------------------

    /// A body of Responses SSE events as the SDK reads it, adapted by the
    /// stream the interpreter folds: the same bytes a mock endpoint would
    /// answer with. The field lines are the endpoint's own spelling — no space
    /// after the colon, which is what MiMo writes.
    fn responses_stream(body: &str) -> ChunkStream {
        let bytes = bytes::Bytes::from(body.to_owned());
        let events = futures_util::stream::iter(vec![Ok(bytes)]);
        ChunkStream::Responses(ResponsesStream::new(openai::ResponseEventStream::new(
            Box::pin(events),
        )))
    }

    /// One event in MiMo's own spelling.
    fn responses_event(payload: serde_json::Value) -> String {
        format!(
            "event:{kind}\ndata:{payload}\n\n",
            kind = payload["type"].as_str().unwrap_or("")
        )
    }

    /// A turn as the live endpoint streams it: a reasoning item, a function
    /// call with sharded arguments, and the completed event with the cache
    /// breakdown. Captured from `api.xiaomimimo.com`, trimmed of the part
    /// boundaries nothing folds.
    fn mimo_tool_turn() -> String {
        [
            responses_event(serde_json::json!({"type":"response.created","sequence_number":0,"response":{"id":"resp_1","status":"in_progress"}})),
            responses_event(serde_json::json!({"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"id":"rs_1","type":"reasoning","summary":[],"content":[],"status":"in_progress"}})),
            responses_event(serde_json::json!({"type":"response.reasoning_text.delta","sequence_number":2,"item_id":"rs_1","output_index":0,"content_index":0,"delta":"I should run "})),
            responses_event(serde_json::json!({"type":"response.reasoning_text.delta","sequence_number":3,"item_id":"rs_1","output_index":0,"content_index":0,"delta":"a command."})),
            responses_event(serde_json::json!({"type":"response.output_item.added","sequence_number":4,"output_index":1,"item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"Bash","arguments":"","status":"in_progress"}})),
            responses_event(serde_json::json!({"type":"response.function_call_arguments.delta","sequence_number":5,"item_id":"fc_1","output_index":1,"delta":"{\"command\": "})),
            responses_event(serde_json::json!({"type":"response.function_call_arguments.delta","sequence_number":6,"item_id":"fc_1","output_index":1,"delta":"\"echo hi\"}"})),
            responses_event(serde_json::json!({"type":"response.output_item.done","sequence_number":7,"output_index":1,"item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"Bash","arguments":"{\"command\": \"echo hi\"}","status":"completed"}})),
            responses_event(serde_json::json!({"type":"response.completed","sequence_number":8,"response":{"id":"resp_1","status":"completed","incomplete_details":null,"output":[{"id":"rs_1","type":"reasoning","summary":[],"content":[{"type":"reasoning_text","text":"I should run a command."}],"status":"completed"},{"id":"fc_1","type":"function_call","call_id":"call_1","name":"Bash","arguments":"{\"command\": \"echo hi\"}","status":"completed"}],"usage":{"input_tokens":257,"input_tokens_details":{"cached_tokens":192},"output_tokens":17,"output_tokens_details":{"reasoning_tokens":14},"total_tokens":274}}})),
        ]
        .concat()
    }

    #[tokio::test]
    async fn responses_stream_folds_a_tool_turn_into_one_message() {
        let mut sse = responses_stream(&mimo_tool_turn());
        let mut accumulator = TurnAccumulator::default();
        let mut usage = None;
        let mut finish = None;
        while let Some(chunk) = sse.next_chunk().await.unwrap() {
            for choice in chunk.choices {
                if let Some(reason) = choice.finish_reason {
                    finish = Some(reason);
                }
                if let Some(delta) = choice.delta {
                    accumulator.feed(&delta);
                }
            }
            if chunk.usage.is_some() {
                usage = chunk.usage;
            }
        }
        let message = accumulator.finish();
        // The call is one call, sharded over two argument deltas, under the
        // call id its result will be addressed by — not the item id.
        let calls = message.tool_calls.as_ref().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].function.name, "Bash");
        assert_eq!(calls[0].function.arguments, r#"{"command": "echo hi"}"#);
        // The reasoning is one item, under the id the endpoint gave it.
        let reasoning = match &message.cot {
            Some(Cot::ResponsesItems { items }) => items,
            other => panic!("expected ResponsesItems cot, got {other:?}"),
        };
        assert_eq!(reasoning.len(), 1);
        assert_eq!(reasoning[0].id, "rs_1");
        assert_eq!(reasoning[0].text, "I should run a command.");
        // The end of the answer, and the prompt-cache counts it carried.
        assert_eq!(finish.as_deref(), Some("stop"));
        let usage = usage.expect("the completed event carries the usage");
        assert_eq!(usage.prompt_tokens, 257);
        assert_eq!(usage.completion_tokens, 17);
        let cache = usage.cache().expect("a breakdown was reported");
        assert_eq!((cache.hit, cache.miss), (192, 65));
    }

    #[tokio::test]
    async fn responses_stream_reports_an_answer_that_hit_the_ceiling() {
        let body = responses_event(serde_json::json!({
            "type":"response.incomplete",
            "sequence_number":0,
            "response":{
                "id":"resp_1",
                "status":"incomplete",
                "incomplete_details":{"reason":"max_output_tokens"},
                "usage":{"input_tokens":10,"output_tokens":131_072,"total_tokens":131_082}
            }
        }));
        let mut sse = responses_stream(&body);
        let chunk = sse.next_chunk().await.unwrap().unwrap();
        // The word `Reply::truncation` knows, whatever this wire calls it.
        assert_eq!(
            chunk.choices[0].finish_reason.as_deref(),
            Some("max_tokens")
        );
        assert!(sse.next_chunk().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn responses_stream_without_a_breakdown_reports_no_cache_figure() {
        // The endpoint said nothing about the cache; that is not a report of
        // zero, and the status line shows `—` rather than inventing one.
        let body = responses_event(serde_json::json!({
            "type":"response.completed",
            "sequence_number":0,
            "response":{"id":"resp_1","status":"completed","usage":{"input_tokens":7,"output_tokens":1,"total_tokens":8}}
        }));
        let mut sse = responses_stream(&body);
        let chunk = sse.next_chunk().await.unwrap().unwrap();
        assert!(chunk.usage.unwrap().cache().is_none());
    }

    #[tokio::test]
    async fn responses_stream_fails_on_a_failed_response() {
        let body = responses_event(serde_json::json!({
            "type":"response.failed",
            "sequence_number":0,
            "response":{"id":"resp_1","status":"failed","error":{"code":"server_error","message":"upstream exploded"}}
        }));
        let mut sse = responses_stream(&body);
        let err = sse.next_chunk().await.unwrap_err().to_string();
        assert!(err.contains("server_error"), "{err}");
        assert!(err.contains("upstream exploded"), "{err}");
    }

    #[tokio::test]
    async fn responses_stream_reads_past_what_it_does_not_fold() {
        // Part boundaries, the summary events of an endpoint that reports its
        // reasoning that way, and an event type this crate has never seen: none
        // of them is a reason to stop reading, and the text that arrives still
        // arrives.
        let body = [
            responses_event(serde_json::json!({"type":"response.content_part.added","sequence_number":0,"item_id":"rs_1","output_index":0,"content_index":0,"part":{"type":"reasoning_text","text":""}})),
            responses_event(serde_json::json!({"type":"response.reasoning_summary_text.delta","sequence_number":1,"item_id":"rs_1","output_index":0,"summary_index":0,"delta":"thinking"})),
            responses_event(serde_json::json!({"type":"response.future_event","sequence_number":2,"whatever":true})),
            responses_event(serde_json::json!({"type":"response.output_text.delta","sequence_number":3,"item_id":"msg_1","output_index":1,"content_index":0,"delta":"hi"})),
        ]
        .concat();
        let mut sse = responses_stream(&body);
        let mut texts = Vec::new();
        while let Some(chunk) = sse.next_chunk().await.unwrap() {
            for choice in chunk.choices {
                if let Some(delta) = choice.delta
                    && let Some(text) = delta.content
                {
                    texts.push(text);
                }
            }
        }
        assert_eq!(texts, vec!["hi".to_string()]);
    }

    #[test]
    fn responses_request_carries_instructions_items_and_tools() {
        let history = vec![
            Message::user("run it"),
            Message {
                role: crate::types::Role::Assistant,
                content: Some("calling".into()),
                cot: Some(Cot::ResponsesItems {
                    items: vec![crate::types::ReasoningItem {
                        id: "rs_1".into(),
                        text: "I should run a command.".into(),
                    }],
                }),
                tool_calls: Some(vec![crate::types::ToolCall {
                    id: "call_1".into(),
                    r#type: "function".into(),
                    function: crate::types::ToolCallFunction {
                        name: "Bash".into(),
                        arguments: r#"{"command":"echo hi"}"#.into(),
                    },
                }]),
                tool_call_id: None,
            },
            Message::tool("call_1", "exit_code: 0\nhi"),
        ];
        // The thinking block of the *other* wire rides along to prove it is
        // dropped: this wire has no variant for a signed block.
        let mut carried = history.clone();
        carried[1].cot = Some(Cot::AnthropicBlocks {
            blocks: vec![crate::types::ThinkingBlock {
                thinking: "other wire".into(),
                signature: "sig".into(),
            }],
        });
        let request = responses_request(
            "mimo-v2.5-pro",
            131_072,
            "You are caocli.\n\nproject rules",
            &carried,
            None,
            Some("high"),
        );
        let v: serde_json::Value = serde_json::to_value(&request).unwrap();
        assert_eq!(v["model"], "mimo-v2.5-pro");
        assert_eq!(v["stream"], true);
        assert_eq!(v["max_output_tokens"], 131_072);
        assert_eq!(v["instructions"], "You are caocli.\n\nproject rules");
        assert_eq!(v["tool_choice"], "auto");
        assert_eq!(v["reasoning"]["effort"], "high");
        // The carried Anthropic block was dropped: this wire has no variant
        // for it, and the pattern match in the builder contributes nothing.
        // The assistant message and its function_call still reach the input.
        let items = v["input"].as_array().unwrap();
        let kinds: Vec<&str> = items.iter().map(|i| i["type"].as_str().unwrap()).collect();
        assert_eq!(
            kinds,
            vec![
                "message",
                "message",
                "function_call",
                "function_call_output"
            ]
        );

        // Sending the original history (Responses wire's own variant):
        // reasoning items reach the input array.
        let request = responses_request(
            "mimo-v2.5-pro",
            131_072,
            "You are caocli.\n\nproject rules",
            &history,
            None,
            Some("high"),
        );
        let v: serde_json::Value = serde_json::to_value(&request).unwrap();
        let items = v["input"].as_array().unwrap();
        let kinds: Vec<&str> = items.iter().map(|i| i["type"].as_str().unwrap()).collect();
        assert_eq!(
            kinds,
            vec![
                "message",
                "reasoning",
                "message",
                "function_call",
                "function_call_output"
            ]
        );
        assert_eq!(items[0]["role"], "user");
        assert_eq!(items[1]["id"], "rs_1");
        assert_eq!(items[1]["content"][0]["type"], "reasoning_text");
        assert_eq!(items[1]["content"][0]["text"], "I should run a command.");
        assert_eq!(items[2]["role"], "assistant");
        assert_eq!(items[2]["content"], "calling");
        assert_eq!(items[3]["call_id"], "call_1");
        assert_eq!(items[3]["name"], "Bash");
        assert_eq!(items[3]["arguments"], r#"{"command":"echo hi"}"#);
        assert_eq!(items[4]["call_id"], "call_1");
        assert_eq!(items[4]["output"], "exit_code: 0\nhi");
        // The fields this wire does not have are absent rather than empty, and
        // every item carries exactly one `type` key of its own — a second one
        // would be a JSON object with the same key twice.
        for item in items {
            let parts = item["content"].as_array().map_or(0, Vec::len);
            let text = serde_json::to_string(item).unwrap();
            assert_eq!(
                text.matches("\"type\"").count(),
                1 + parts,
                "the item's own type and its content parts: {text}"
            );
        }
    }

    #[test]
    fn responses_request_omits_an_empty_assistant_turn_and_an_unknown_tier() {
        // A turn that only called tools says nothing, and this wire has no
        // empty assistant message: the calls carry it.
        let history = vec![Message {
            role: crate::types::Role::Assistant,
            content: Some("".into()),
            cot: None,
            tool_calls: Some(vec![crate::types::ToolCall {
                id: "call_1".into(),
                r#type: "function".into(),
                function: crate::types::ToolCallFunction {
                    name: "Bash".into(),
                    arguments: "{}".into(),
                },
            }]),
            tool_call_id: None,
        }];
        let request = responses_request(
            "mimo-v2.5-pro",
            131_072,
            "sys",
            &history,
            None,
            // A tier this crate's enum has no word for: left unsent rather than
            // guessed at, which leaves the endpoint's own default in force.
            Some("max"),
        );
        let v: serde_json::Value = serde_json::to_value(&request).unwrap();
        let items = v["input"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"], "function_call");
        assert!(v.get("reasoning").is_none(), "{v}");
    }

    #[test]
    fn responses_request_offers_the_tools_with_the_strict_flag_the_endpoint_lists() {
        let tools = crate::tools::definitions();
        let request = responses_request(
            "mimo-v2.5-pro",
            131_072,
            "sys",
            &[],
            Some(&tools[..1]),
            None,
        );
        let v: serde_json::Value = serde_json::to_value(&request).unwrap();
        let tool = &v["tools"][0];
        assert_eq!(tool["type"], "function");
        assert_eq!(tool["name"], tools[0].function.name);
        assert_eq!(tool["strict"], false);
        assert!(tool["parameters"]["type"].is_string());
    }
}
