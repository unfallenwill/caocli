//! The Messages API as types.
//!
//! Two halves that never meet. The **request** half is serialized and never
//! parsed: its field names are the API's field names, so a glance at a type is
//! a glance at the JSON. The **response** half is parsed and never serialized,
//! and it is tolerant by construction — the spec says event types and enum
//! values may be added, so every enum has a catch-all and unknown object fields
//! are ignored rather than refused.
//!
//! What is here is the request and response vocabulary a caller can reach
//! without a beta header, and the fields this workspace sends. What is
//! deliberately not here, with the reason: **server tools** (`web_search`,
//! `code_execution`, tool search) and the fields that belong to them
//! (`defer_loading`, `allowed_callers`, `toolset_name`, `caller`) — a call this
//! crate cannot run is a type nobody can check; **containers**, which hold state
//! this program does not keep; **structured outputs**
//! (`output_config.format`), which is a feature of its own; and the batch and
//! files APIs. A response carrying one of them still reads: every enum has a
//! catch-all, and unknown object fields are ignored.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ============================================================================
// Request
// ============================================================================

/// The `POST /v1/messages` body.
///
/// Built rather than assembled: `new` takes the three fields the spec requires,
/// and every other field has a `with_…` that names what it does on the wire, so
/// a request cannot carry a field nobody meant to send. Nothing here is a
/// default: an omitted field is absent from the JSON, not null, because the two
/// are not the same request.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MessagesRequest {
    /// The model id.
    pub model: String,
    /// Ceiling on the answer, thinking included. Every token the model emits in
    /// this turn counts against it.
    pub max_tokens: u32,
    /// The conversation, oldest first.
    pub messages: Vec<MessageParam>,
    /// The system prompt: a top-level field on this wire, not a message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<SystemPrompt>,
    /// The tools the model may call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
    /// How the model should use those tools.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    /// Extended thinking: whether the model reasons before answering, and how
    /// much of that reasoning comes back.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,
    /// How hard the model works: `output_config.effort`. Affects every output
    /// token, thinking and tool arguments included.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_config: Option<OutputConfig>,
    /// Prompt cache, the automatic form: one breakpoint that the server keeps
    /// at the end of the cacheable prefix and moves forward as the conversation
    /// grows. It costs one of the four breakpoint slots a request may have.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
    /// Whether to answer as a stream of events.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    /// Which capacity serves the request: priority where it is available, or
    /// standard only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<ServiceTier>,
    /// Where the request is processed. Naming a region overrides the
    /// workspace's own default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inference_geo: Option<String>,
    /// Strings that end the answer when the model produces them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,
    /// Who made the request, for the endpoint's own accounting.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
}

impl MessagesRequest {
    /// The required shape and nothing else: a model, a ceiling, a conversation.
    pub fn new(model: impl Into<String>, max_tokens: u32, messages: Vec<MessageParam>) -> Self {
        Self {
            model: model.into(),
            max_tokens,
            messages,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            cache_control: None,
            stream: None,
            service_tier: None,
            inference_geo: None,
            stop_sequences: None,
            metadata: None,
        }
    }

    /// Answer with a stream of events rather than one object.
    pub fn streaming(mut self) -> Self {
        self.stream = Some(true);
        self
    }

    /// The system prompt, sent as one block of text.
    pub fn with_system(mut self, text: impl Into<String>) -> Self {
        self.system = Some(SystemPrompt::Text(text.into()));
        self
    }

    /// The system prompt as blocks, which is the form that can carry
    /// [`CacheControl`] breakpoints inside it.
    pub fn with_system_blocks(mut self, blocks: Vec<Block>) -> Self {
        self.system = Some(SystemPrompt::Blocks(blocks));
        self
    }

    /// The tools the model may call.
    pub fn with_tools(mut self, tools: Vec<Tool>) -> Self {
        self.tools = Some(tools);
        self
    }

    /// How the model should choose among those tools.
    pub fn with_tool_choice(mut self, choice: ToolChoice) -> Self {
        self.tool_choice = Some(choice);
        self
    }

    /// Extended thinking.
    pub fn with_thinking(mut self, thinking: ThinkingConfig) -> Self {
        self.thinking = Some(thinking);
        self
    }

    /// How hard the model works.
    pub fn with_effort(mut self, effort: Effort) -> Self {
        self.output_config = Some(OutputConfig {
            effort: Some(effort),
        });
        self
    }

    /// Turn the prompt cache on with the automatic breakpoint. See
    /// [`MessagesRequest::cache_control`].
    pub fn with_automatic_cache(mut self) -> Self {
        self.cache_control = Some(CacheControl::ephemeral());
        self
    }

    /// Who made the request.
    pub fn with_metadata(mut self, metadata: Metadata) -> Self {
        self.metadata = Some(metadata);
        self
    }

    /// Which capacity should serve the request.
    pub fn with_service_tier(mut self, tier: ServiceTier) -> Self {
        self.service_tier = Some(tier);
        self
    }

    /// Where the request should be processed.
    pub fn with_inference_geo(mut self, region: impl Into<String>) -> Self {
        self.inference_geo = Some(region.into());
        self
    }
}

/// Which capacity serves a request.
///
/// The answer's own `usage.service_tier` is a different vocabulary
/// (`standard`, `priority`, `batch`): this is what a caller *asks* for, and
/// that is what it got.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceTier {
    /// Priority capacity where the workspace has it, standard otherwise.
    Auto,
    /// Standard capacity only.
    StandardOnly,
}

/// Who the request is attributed to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Metadata {
    /// An opaque identifier for the end user. Must not be a name, an email
    /// address or anything else that identifies a person.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
}

/// Who wrote a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// The person, or whatever speaks for them.
    User,
    /// The model.
    Assistant,
    /// A turn of instructions inside the conversation, where a caller needs
    /// one to sit in the history rather than in the top-level prompt.
    System,
}

/// One turn of the conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageParam {
    /// Who wrote it.
    pub role: Role,
    /// What it says: the plain string a text turn is, or the blocks a turn with
    /// images, tool calls or tool results needs.
    pub content: MessageContent,
}

impl MessageParam {
    /// A turn that is one run of text. A string rather than a one-block array,
    /// because that is what the spec writes and what the prefix cache hashes.
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: MessageContent::Text(text.into()),
        }
    }

    /// A turn of blocks.
    pub fn blocks(role: Role, blocks: Vec<Block>) -> Self {
        Self {
            role,
            content: MessageContent::Blocks(blocks),
        }
    }
}

/// A message's content: a string, or blocks. Untagged, the string tried first,
/// so a text turn is written back as the string it was.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    /// Plain text.
    Text(String),
    /// Structured content.
    Blocks(Vec<Block>),
}

/// The system prompt: one string, or blocks when a cache breakpoint has to sit
/// inside it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SystemPrompt {
    /// One block of text.
    Text(String),
    /// The blocks the prompt is made of.
    Blocks(Vec<Block>),
}

// ============================================================================
// Content blocks (the request and the answer share this vocabulary)
// ============================================================================

/// One content block: what it is, plus the cache breakpoint that may sit on it.
///
/// The breakpoint is a field of the block rather than of its kind because that
/// is where the wire puts it — any cacheable block may carry one, and a block
/// that carries one is a place the server may cut the cached prefix at.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Block {
    /// What the block is.
    #[serde(flatten)]
    pub kind: BlockKind,
    /// A prompt cache breakpoint after this block. Four per request at most.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

impl Block {
    /// A block of text.
    pub fn text(text: impl Into<String>) -> Self {
        Self::of(BlockKind::Text { text: text.into() })
    }

    /// An image, from a base64 payload or a URL.
    pub fn image(source: ImageSource) -> Self {
        Self::of(BlockKind::Image { source })
    }

    /// A call the model made, replayed so the result that follows it has
    /// something to answer.
    pub fn tool_use(id: impl Into<String>, name: impl Into<String>, input: Value) -> Self {
        Self::of(BlockKind::ToolUse {
            id: id.into(),
            name: name.into(),
            input,
        })
    }

    /// The result of such a call.
    pub fn tool_result(tool_use_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self::of(BlockKind::ToolResult {
            tool_use_id: tool_use_id.into(),
            content: Some(ToolResultContent::Text(content.into())),
            is_error: false,
        })
    }

    /// The whole of [`Block::tool_result`], with every field settable: a result
    /// that is blocks rather than text, and a result that reports the tool's
    /// own failure.
    pub fn tool_result_full(
        tool_use_id: impl Into<String>,
        content: ToolResultContent,
        is_error: bool,
    ) -> Self {
        Self::of(BlockKind::ToolResult {
            tool_use_id: tool_use_id.into(),
            content: Some(content),
            is_error,
        })
    }

    /// A reasoning block, replayed verbatim — the signature included — because
    /// the endpoint checks it before it will use the reasoning again.
    pub fn thinking(thinking: impl Into<String>, signature: impl Into<String>) -> Self {
        Self::of(BlockKind::Thinking {
            thinking: thinking.into(),
            signature: signature.into(),
        })
    }

    /// A reasoning block the endpoint encrypted and asked to have back
    /// unread: opaque, and only ever replayed.
    pub fn redacted_thinking(data: impl Into<String>) -> Self {
        Self::of(BlockKind::RedactedThinking { data: data.into() })
    }

    fn of(kind: BlockKind) -> Self {
        Self {
            kind,
            cache_control: None,
        }
    }

    /// Put a cache breakpoint after this block.
    pub fn with_cache_control(mut self, cache_control: CacheControl) -> Self {
        self.cache_control = Some(cache_control);
        self
    }
}

/// What a content block is. The tag is the API's `type` field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BlockKind {
    /// A run of text.
    Text {
        /// The text. Empty at a block's start; the words arrive as deltas.
        #[serde(default)]
        text: String,
    },
    /// An image.
    Image {
        /// Where the picture is.
        source: ImageSource,
    },
    /// A tool the model called.
    ToolUse {
        /// The id the model gave the call, which its result quotes back.
        id: String,
        /// The tool's name.
        #[serde(default)]
        name: String,
        /// The call's arguments. Empty at the block's start; they arrive as
        /// argument shards, and a block that never carried any stays an empty
        /// object rather than becoming null.
        #[serde(default = "empty_object")]
        input: Value,
    },
    /// The result of such a call, in the user turn that answers it.
    ToolResult {
        /// The id of the call this answers.
        tool_use_id: String,
        /// What the tool produced.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<ToolResultContent>,
        /// Whether the tool failed. A failure reported as a plain result reads
        /// to the model as an answer it should work from.
        #[serde(default, skip_serializing_if = "is_false")]
        is_error: bool,
    },
    /// The model's reasoning, with the signature that closes the block.
    Thinking {
        /// The reasoning text, empty when the endpoint was asked to omit it, and
        /// empty at the block's start — the text arrives as thinking deltas.
        #[serde(default)]
        thinking: String,
        /// The opaque signature. Replayed unchanged; never read. Absent until
        /// the signature delta that closes the block.
        #[serde(default)]
        signature: String,
    },
    /// Reasoning the endpoint encrypted and will not show.
    RedactedThinking {
        /// The opaque payload, replayed unchanged.
        #[serde(default)]
        data: String,
    },
    /// A block this crate does not model, from a message it parsed.
    ///
    /// Never produced by a request: what is not modelled here is not sent from
    /// here either. It exists so that an answer carrying a block type the spec
    /// added later is read as the blocks around it, rather than failing the
    /// whole message.
    #[serde(other)]
    Unknown,
}

/// What a tool result carries: text, or blocks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolResultContent {
    /// The result as one string.
    Text(String),
    /// The result as blocks — text and images.
    Blocks(Vec<Block>),
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// An empty JSON object, where the wire means one and a missing field would
/// otherwise be null: `value["key"]` on a null panics, and on an object it is
/// what the caller asked for.
fn empty_object() -> Value {
    Value::Object(serde_json::Map::new())
}

/// Where an image block reads its picture. A `data:` URL's bytes are split out
/// by the caller; the endpoint takes either form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImageSource {
    /// The bytes themselves, base64-encoded.
    Base64 {
        /// The image's media type, such as `image/png`.
        media_type: String,
        /// The base64 payload.
        data: String,
    },
    /// A URL the endpoint fetches the picture from.
    Url {
        /// Where the picture is.
        url: String,
    },
}

// ============================================================================
// Tools
// ============================================================================

/// A tool the model may call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tool {
    /// The tool's name, matching `^[a-zA-Z0-9_-]{1,128}$`.
    pub name: String,
    /// What the tool does and when to use it. Optional to the wire, and the
    /// difference between a tool the model uses well and one it ignores.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The JSON schema of the arguments the model may produce.
    pub input_schema: Value,
    /// Example argument objects, to show the shape rather than describe it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_examples: Option<Vec<Value>>,
    /// Whether the endpoint should constrain the model's arguments to the
    /// schema rather than merely asking for them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
    /// A cache breakpoint after this tool definition — where a long tool list
    /// belongs, since it is the same bytes on every request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

impl Tool {
    /// A tool with a name, a description and a schema.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: Some(description.into()),
            input_schema,
            input_examples: None,
            strict: None,
            cache_control: None,
        }
    }

    /// Put a cache breakpoint after this definition.
    pub fn with_cache_control(mut self, cache_control: CacheControl) -> Self {
        self.cache_control = Some(cache_control);
        self
    }
}

/// How the model should use the tools it was given.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChoice {
    /// Decide by itself. The default when tools are present.
    Auto {
        /// Refuse more than one call per turn.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        disable_parallel_tool_use: Option<bool>,
    },
    /// Call some tool, whichever it judges best.
    Any {
        /// Refuse more than one call per turn.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        disable_parallel_tool_use: Option<bool>,
    },
    /// Call this tool.
    Tool {
        /// The tool's name.
        name: String,
        /// Refuse more than one call per turn.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        disable_parallel_tool_use: Option<bool>,
    },
    /// Call no tool this turn.
    None,
}

impl ToolChoice {
    /// Let the model decide.
    pub fn auto() -> Self {
        ToolChoice::Auto {
            disable_parallel_tool_use: None,
        }
    }

    /// Not this turn: answer in text.
    pub fn none() -> Self {
        ToolChoice::None
    }
}

// ============================================================================
// Thinking, effort, cache
// ============================================================================

/// How much of the model's reasoning comes back, and under which configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ThinkingConfig {
    /// Reasoning with a fixed token budget. The older mode: deprecated on the
    /// models that have adaptive thinking, and refused outright by the newest
    /// ones, where the budget belongs to [`Effort`] instead.
    Enabled {
        /// How many tokens the model may reason with. At least 1024, and less
        /// than `max_tokens`, since it is spent from the same ceiling.
        budget_tokens: u32,
        /// Whether the reasoning comes back as text.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display: Option<ThinkingDisplay>,
    },
    /// The model decides whether and how deeply to think, guided by
    /// [`Effort`].
    Adaptive {
        /// Whether the reasoning comes back as text.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display: Option<ThinkingDisplay>,
    },
    /// No reasoning. Some models refuse this at high effort; the instruction is
    /// a request, not a guarantee.
    Disabled,
}

impl ThinkingConfig {
    /// Reasoning, its depth left to the model and the effort level.
    pub fn adaptive() -> Self {
        ThinkingConfig::Adaptive { display: None }
    }

    /// Reasoning with a fixed budget.
    pub fn enabled(budget_tokens: u32) -> Self {
        ThinkingConfig::Enabled {
            budget_tokens,
            display: None,
        }
    }

    /// Thinking off.
    pub fn disabled() -> Self {
        ThinkingConfig::Disabled
    }

    /// Ask for the reasoning text itself.
    ///
    /// Worth setting explicitly: the newest models default to `omitted`, where
    /// only the signature comes back and a front end that shows reasoning has
    /// nothing to show. It is also the faster setting when the text is not
    /// wanted, since the server then streams no thinking tokens at all.
    pub fn summarized(self) -> Self {
        let display = Some(ThinkingDisplay::Summarized);
        match self {
            ThinkingConfig::Enabled { budget_tokens, .. } => ThinkingConfig::Enabled {
                budget_tokens,
                display,
            },
            ThinkingConfig::Adaptive { .. } => ThinkingConfig::Adaptive { display },
            ThinkingConfig::Disabled => ThinkingConfig::Disabled,
        }
    }
}

/// Whether thinking text is returned, or only the signature that stands for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingDisplay {
    /// The reasoning text comes back.
    Summarized,
    /// The reasoning text is withheld; the signature still comes back, so
    /// multi-turn continuity is unaffected.
    Omitted,
}

/// `output_config`: how hard the model works on this request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputConfig {
    /// The level. Absent leaves the model's own default in force.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
}

/// How many tokens the model is willing to spend on the answer, thinking
/// included.
///
/// Not every model serves every level, and the endpoint refuses a level it does
/// not serve rather than rounding it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    /// The most economical: speed and cost first.
    Low,
    /// A balance of cost and capability.
    Medium,
    /// The endpoint's own default.
    High,
    /// Beyond high, for long-horizon work.
    Xhigh,
    /// No constraint on token spending.
    Max,
}

impl fmt::Display for Effort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Effort {
    /// The level as the wire spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::Xhigh => "xhigh",
            Effort::Max => "max",
        }
    }

    /// The level a name stands for, for a caller holding names — a configured
    /// list of the levels an endpoint serves, say. `None` for a name this crate
    /// does not know, which is a level to leave unsent rather than to guess at.
    pub fn from_name(name: &str) -> Option<Self> {
        [
            Effort::Low,
            Effort::Medium,
            Effort::High,
            Effort::Xhigh,
            Effort::Max,
        ]
        .into_iter()
        .find(|level| level.as_str() == name)
    }
}

/// A prompt cache breakpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheControl {
    /// The only cache type the spec defines.
    pub r#type: CacheType,
    /// How long the entry lives. The default is five minutes; an hour costs
    /// more to write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<Ttl>,
}

impl CacheControl {
    /// The standard breakpoint: the default five-minute entry.
    pub fn ephemeral() -> Self {
        Self {
            r#type: CacheType::Ephemeral,
            ttl: None,
        }
    }

    /// An hour-long entry, written at a higher price.
    pub fn ephemeral_hour() -> Self {
        Self {
            r#type: CacheType::Ephemeral,
            ttl: Some(Ttl::OneHour),
        }
    }
}

/// The cache types the spec defines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheType {
    /// Written on demand, read until it expires.
    Ephemeral,
}

/// How long a cache entry lasts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Ttl {
    /// Five minutes, the default.
    #[serde(rename = "5m")]
    FiveMinutes,
    /// One hour, at a higher write price.
    #[serde(rename = "1h")]
    OneHour,
}

// ============================================================================
// Response
// ============================================================================

/// A complete message, and the `message` of a `message_start` event.
///
/// Fields this crate does not model are ignored, not refused: the spec reserves
/// the right to add to an object.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Message {
    /// The message's id, which a later request may refer to.
    pub id: String,
    /// The model that answered. On a routed endpoint this may differ from the
    /// one that was asked for.
    #[serde(default)]
    pub model: String,
    /// What the message is made of, in the order it was written.
    #[serde(default)]
    pub content: Vec<Block>,
    /// Why the model stopped. `None` until the stream's last `message_delta`.
    #[serde(default)]
    pub stop_reason: Option<StopReason>,
    /// The stop sequence that ended it, when one did.
    #[serde(default)]
    pub stop_sequence: Option<String>,
    /// What the endpoint attached to a refusal, carried through unread — the
    /// same shape the `message_delta` of a stream carries.
    #[serde(default)]
    pub stop_details: Option<Value>,
    /// The token counts. On a stream this is the `message_start` report, which
    /// is not the whole story — see [`Usage`].
    #[serde(default)]
    pub usage: Usage,
}

/// Why the model stopped.
///
/// The values are the endpoint's, so a value this crate does not know is
/// [`StopReason::Other`] rather than a parse failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// It finished answering.
    EndTurn,
    /// It hit `max_tokens`: the answer is cut off mid-thought, and the one
    /// thing a caller must not do is treat it as complete.
    MaxTokens,
    /// It produced one of the `stop_sequences`.
    StopSequence,
    /// It is waiting for tool results.
    ToolUse,
    /// A server tool needs another round; the answer so far is to be sent back
    /// as it stands.
    PauseTurn,
    /// It declined to answer.
    Refusal,
    /// The conversation no longer fits the model's context window.
    ModelContextWindowExceeded,
    /// A reason this crate does not know.
    #[serde(other)]
    Other,
}

impl StopReason {
    /// The reason as the wire spells it.
    pub fn as_str(&self) -> &'static str {
        match self {
            StopReason::EndTurn => "end_turn",
            StopReason::MaxTokens => "max_tokens",
            StopReason::StopSequence => "stop_sequence",
            StopReason::ToolUse => "tool_use",
            StopReason::PauseTurn => "pause_turn",
            StopReason::Refusal => "refusal",
            StopReason::ModelContextWindowExceeded => "model_context_window_exceeded",
            StopReason::Other => "other",
        }
    }
}

impl fmt::Display for StopReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The token counts, as one event reported them.
///
/// Every field is optional, and that is the point: a count the event did not
/// carry is `None`, which is not the same as a count it carried as zero. The
/// two ends of this wire disagree about where the prompt is reported — one
/// names it in `message_start`, another reports zeros there and the real
/// figures in `message_delta` — so a reader that cannot tell "absent" from
/// "zero" reads one of them as no caching at all.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Usage {
    /// The input tokens that were not served from cache.
    #[serde(default)]
    pub input_tokens: Option<u64>,
    /// The tokens the model wrote, thinking included.
    #[serde(default)]
    pub output_tokens: Option<u64>,
    /// Tokens written into the cache by this request.
    #[serde(default)]
    pub cache_creation_input_tokens: Option<u64>,
    /// Tokens served from the cache by this request.
    #[serde(default)]
    pub cache_read_input_tokens: Option<u64>,
    /// The cache writes, split by how long the entry lasts.
    #[serde(default)]
    pub cache_creation: Option<CacheCreation>,
    /// The answer's own breakdown.
    #[serde(default)]
    pub output_tokens_details: Option<OutputTokensDetails>,
    /// Which service tier served the request: `standard`, `priority` or
    /// `batch` — the answer's vocabulary, not the request's.
    #[serde(default)]
    pub service_tier: Option<String>,
    /// The region the request was processed in.
    #[serde(default)]
    pub inference_geo: Option<String>,
}

/// The cache writes of one request, by entry lifetime.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub struct CacheCreation {
    /// Tokens written into a one-hour entry.
    #[serde(default)]
    pub ephemeral_1h_input_tokens: u64,
    /// Tokens written into a five-minute entry.
    #[serde(default)]
    pub ephemeral_5m_input_tokens: u64,
}

/// What the answer's tokens were spent on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub struct OutputTokensDetails {
    /// How many of them were reasoning.
    #[serde(default)]
    pub thinking_tokens: u64,
}

impl Usage {
    /// Fold a later report into this one: every count the later report carries
    /// replaces the earlier value, and one it leaves out keeps it.
    ///
    /// This is the whole reason the fields are optional. The events of one
    /// stream each report part of the picture, and which part differs by
    /// endpoint; presence is the only signal that survives the difference.
    pub fn merge(&mut self, later: &Usage) {
        macro_rules! take {
            ($($field:ident),* $(,)?) => {
                $(if later.$field.is_some() {
                    self.$field = later.$field.clone();
                })*
            };
        }
        take!(
            input_tokens,
            output_tokens,
            cache_creation_input_tokens,
            cache_read_input_tokens,
            cache_creation,
            output_tokens_details,
            service_tier,
            inference_geo,
        );
    }

    /// The whole prompt: what was written into the cache, what was read from
    /// it, and what was neither.
    pub fn prompt_tokens(&self) -> u64 {
        self.input_tokens.unwrap_or(0)
            + self.cache_read_input_tokens.unwrap_or(0)
            + self.cache_creation_input_tokens.unwrap_or(0)
    }

    /// The prompt and the answer together.
    pub fn total_tokens(&self) -> u64 {
        self.prompt_tokens() + self.output_tokens.unwrap_or(0)
    }

    /// The reasoning tokens of the answer, when the endpoint broke them out.
    pub fn thinking_tokens(&self) -> Option<u64> {
        self.output_tokens_details.map(|d| d.thinking_tokens)
    }

    /// The cache reads and writes, when the endpoint reported either. `None`
    /// when it reported neither, so a caller shows "no caching" rather than an
    /// invented zero.
    pub fn cache(&self) -> Option<(u64, u64)> {
        match (
            self.cache_read_input_tokens,
            self.cache_creation_input_tokens,
        ) {
            (None, None) => None,
            (read, created) => Some((read.unwrap_or(0), created.unwrap_or(0))),
        }
    }
}

// ============================================================================
// The event stream
// ============================================================================

/// One event of a streaming answer.
///
/// The spec adds event types over time and tells clients to handle an unknown
/// one gracefully, so an event this crate does not model arrives as
/// [`Event::Unknown`] and the stream reads on.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// The answer opens: the message's id, model, and the prompt's counts.
    MessageStart {
        /// The message as it stands — its content still empty.
        message: Message,
    },
    /// A content block opens. Its `index` is where it sits in the final
    /// content, which is how its deltas are matched to it.
    ContentBlockStart {
        /// The block's position in the answer.
        index: u32,
        /// The block, with whatever it already knows — a tool call's id and
        /// name, for instance.
        content_block: Block,
    },
    /// A block grows.
    ContentBlockDelta {
        /// Which block.
        index: u32,
        /// What was added to it.
        delta: BlockDelta,
    },
    /// A block is complete.
    ContentBlockStop {
        /// Which block.
        index: u32,
    },
    /// The answer's top level changed: why it stopped, and how much it cost.
    /// May arrive more than once, and the last one carries the final counts.
    MessageDelta {
        /// What changed.
        delta: MessageDeltaBody,
        /// The counts as of this event. See [`Usage::merge`].
        #[serde(default)]
        usage: Option<Usage>,
    },
    /// The answer is over. This is the end of the stream.
    MessageStop,
    /// A keep-alive. Carries nothing.
    Ping,
    /// The stream failed. There is no more of the answer coming.
    Error {
        /// What failed.
        error: crate::StreamError,
    },
    /// An event type this crate does not model.
    #[serde(other)]
    Unknown,
}

/// What a `message_delta` changed.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MessageDeltaBody {
    /// Why the model stopped. Absent on an interim event.
    #[serde(default)]
    pub stop_reason: Option<StopReason>,
    /// The stop sequence that ended it.
    #[serde(default)]
    pub stop_sequence: Option<String>,
    /// Anything else the endpoint attached to the stop reason, carried through
    /// unread.
    #[serde(default)]
    pub stop_details: Option<Value>,
}

/// What a `content_block_delta` added to its block.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BlockDelta {
    /// Text, appended to the block's text.
    TextDelta {
        /// The text added.
        text: String,
    },
    /// Reasoning, appended to the block's reasoning.
    ThinkingDelta {
        /// The reasoning added.
        thinking: String,
    },
    /// The signature that closes a reasoning block, sent just before the block
    /// stops.
    SignatureDelta {
        /// The opaque signature.
        signature: String,
    },
    /// A shard of a tool call's arguments — an arbitrary cut through the JSON,
    /// not a value: the shards are concatenated and parsed as one string.
    InputJsonDelta {
        /// The shard.
        partial_json: String,
    },
    /// A citation attached to the text just streamed.
    CitationsDelta {
        /// The citation, carried through unread.
        citation: Value,
    },
    /// A delta type this crate does not model.
    #[serde(other)]
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn json_of<T: Serialize>(value: &T) -> Value {
        serde_json::to_value(value).expect("a request type serializes")
    }

    fn parse_event(line: &str) -> Event {
        serde_json::from_str(line).expect("an event parses")
    }

    // ---------------------------------------------------------------- request

    #[test]
    fn the_required_shape_is_all_new_sends() {
        let request = MessagesRequest::new("m", 1024, vec![MessageParam::user("hi")]);
        assert_eq!(
            json_of(&request),
            json!({
                "model": "m",
                "max_tokens": 1024,
                "messages": [{"role": "user", "content": "hi"}],
            })
        );
    }

    #[test]
    fn streaming_is_a_field_the_caller_asks_for() {
        let request = MessagesRequest::new("m", 1024, vec![MessageParam::user("hi")]).streaming();
        let v = json_of(&request);
        assert_eq!(v["stream"], true);
    }

    #[test]
    fn a_fully_built_request_writes_every_field_the_spec_names() {
        let request = MessagesRequest::new(
            "MiniMax-M3",
            131_072,
            vec![
                MessageParam::user("look at this"),
                MessageParam::blocks(
                    Role::Assistant,
                    vec![
                        Block::thinking("weighing it", "sig-1"),
                        Block::tool_use("call_1", "Read", json!({"file_path": "/x"})),
                    ],
                ),
                MessageParam::blocks(Role::User, vec![Block::tool_result("call_1", "the file")]),
            ],
        )
        .streaming()
        .with_system("system prompt")
        .with_tools(vec![Tool::new(
            "Read",
            "read a file",
            json!({"type": "object"}),
        )])
        .with_tool_choice(ToolChoice::auto())
        .with_thinking(ThinkingConfig::adaptive())
        .with_effort(Effort::Medium)
        .with_automatic_cache()
        .with_metadata(Metadata {
            user_id: Some("u1".into()),
        });

        assert_eq!(
            json_of(&request),
            json!({
                "model": "MiniMax-M3",
                "max_tokens": 131_072,
                "messages": [
                    {"role": "user", "content": "look at this"},
                    {"role": "assistant", "content": [
                        {"type": "thinking", "thinking": "weighing it", "signature": "sig-1"},
                        {"type": "tool_use", "id": "call_1", "name": "Read",
                         "input": {"file_path": "/x"}},
                    ]},
                    {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "call_1",
                         "content": "the file"},
                    ]},
                ],
                "system": "system prompt",
                "tools": [{"name": "Read", "description": "read a file",
                           "input_schema": {"type": "object"}}],
                "tool_choice": {"type": "auto"},
                "thinking": {"type": "adaptive"},
                "output_config": {"effort": "medium"},
                "cache_control": {"type": "ephemeral"},
                "stream": true,
                "metadata": {"user_id": "u1"},
            })
        );
    }

    #[test]
    fn an_omitted_field_is_absent_rather_than_null() {
        // The difference matters: a null is a field that was sent, and the
        // endpoints do not all read the two the same way.
        let v = json_of(&MessagesRequest::new("m", 1, vec![]));
        for absent in [
            "system",
            "tools",
            "tool_choice",
            "thinking",
            "output_config",
            "cache_control",
            "stream",
            "service_tier",
            "inference_geo",
            "stop_sequences",
            "metadata",
        ] {
            assert!(v.get(absent).is_none(), "{absent} was written: {v}");
        }
        // And the two a caller would reach for from an older client are not
        // fields this wire has at all: sampling is the model's own business
        // here, and a request that names a temperature is a 400.
        for gone in ["temperature", "top_p"] {
            assert!(v.get(gone).is_none(), "{gone} is not a field of this wire");
        }
    }

    // ----------------------------------------------------------------- blocks

    #[test]
    fn a_cache_breakpoint_rides_on_the_block_not_beside_it() {
        let block = Block::text("cacheable").with_cache_control(CacheControl::ephemeral_hour());
        assert_eq!(
            json_of(&block),
            json!({
                "type": "text",
                "text": "cacheable",
                "cache_control": {"type": "ephemeral", "ttl": "1h"},
            })
        );
    }

    #[test]
    fn every_block_kind_writes_the_shape_the_wire_expects() {
        assert_eq!(
            json_of(&Block::tool_use("call_1", "Bash", json!({"command": "ls"}))),
            json!({"type": "tool_use", "id": "call_1", "name": "Bash",
                   "input": {"command": "ls"}})
        );
        assert_eq!(
            json_of(&Block::tool_result("call_1", "output")),
            json!({"type": "tool_result", "tool_use_id": "call_1", "content": "output"})
        );
        // A failed tool says so, which is how the model tells a broken tool
        // from a tool whose answer happens to read like an error.
        assert_eq!(
            json_of(&Block::tool_result_full(
                "call_1",
                ToolResultContent::Text("boom".into()),
                true
            )),
            json!({"type": "tool_result", "tool_use_id": "call_1", "content": "boom",
                   "is_error": true})
        );
        // A result may be blocks, images included.
        assert_eq!(
            json_of(&Block::tool_result_full(
                "call_1",
                ToolResultContent::Blocks(vec![Block::text("see this")]),
                false
            )),
            json!({"type": "tool_result", "tool_use_id": "call_1",
                   "content": [{"type": "text", "text": "see this"}]})
        );
        assert_eq!(
            json_of(&Block::thinking("hmm", "sig")),
            json!({"type": "thinking", "thinking": "hmm", "signature": "sig"})
        );
        assert_eq!(
            json_of(&Block::redacted_thinking("opaque")),
            json!({"type": "redacted_thinking", "data": "opaque"})
        );
        assert_eq!(
            json_of(&Block::image(ImageSource::Base64 {
                media_type: "image/png".into(),
                data: "AAAA".into(),
            })),
            json!({"type": "image", "source": {"type": "base64",
                   "media_type": "image/png", "data": "AAAA"}})
        );
        assert_eq!(
            json_of(&Block::image(ImageSource::Url {
                url: "https://x/y.png".into(),
            })),
            json!({"type": "image", "source": {"type": "url", "url": "https://x/y.png"}})
        );
    }

    #[test]
    fn a_system_prompt_may_be_blocks_when_a_breakpoint_sits_inside_it() {
        let request = MessagesRequest::new("m", 1, vec![]).with_system_blocks(vec![
            Block::text("first").with_cache_control(CacheControl::ephemeral()),
        ]);
        let v = json_of(&request);
        assert_eq!(
            v["system"],
            json!([{"type": "text", "text": "first", "cache_control": {"type": "ephemeral"}}])
        );
    }

    #[test]
    fn a_message_may_be_a_system_turn_and_a_service_tier_names_its_capacity() {
        // The role vocabulary runs to system turns as well as the two ends of
        // the conversation.
        let role: Role = serde_json::from_str(r#""system""#).unwrap();
        assert_eq!(role, Role::System);
        for (role, spelled) in [
            (Role::User, "user"),
            (Role::Assistant, "assistant"),
            (Role::System, "system"),
        ] {
            assert_eq!(json_of(&role), json!(spelled));
        }

        // The request's own tier vocabulary: what it asks for.
        for (tier, spelled) in [
            (ServiceTier::Auto, "auto"),
            (ServiceTier::StandardOnly, "standard_only"),
        ] {
            assert_eq!(json_of(&tier), json!(spelled));
            let request = MessagesRequest::new("m", 1, vec![]).with_service_tier(tier);
            assert_eq!(json_of(&request)["service_tier"], json!(spelled));
        }

        let request = MessagesRequest::new("m", 1, vec![]).with_inference_geo("eu");
        assert_eq!(json_of(&request)["inference_geo"], json!("eu"));
    }

    #[test]
    fn an_output_config_without_an_effort_is_a_field_with_nothing_in_it() {
        // Every part of `output_config` is optional — `format` is the other
        // field it carries — so a caller may send the object and no level, and
        // the request is still a request the spec describes.
        assert_eq!(json_of(&OutputConfig::default()), json!({}));
        assert_eq!(
            json_of(&OutputConfig {
                effort: Some(Effort::Max)
            }),
            json!({"effort": "max"})
        );
    }

    #[test]
    fn is_error_false_is_not_written_at_all() {
        // The wire's default is false; sending it is a field the endpoint has
        // to read for nothing.
        let v = json_of(&Block::tool_result("call_1", "fine"));
        assert!(v.get("is_error").is_none(), "{v}");
    }

    // ------------------------------------------------------------------ tools

    #[test]
    fn a_tool_definition_carries_only_what_the_caller_set() {
        let tool = Tool::new("Read", "read a file", json!({"type": "object"}));
        assert_eq!(
            json_of(&tool),
            json!({"name": "Read", "description": "read a file",
                   "input_schema": {"type": "object"}})
        );
        let cached = tool.with_cache_control(CacheControl::ephemeral());
        assert_eq!(
            json_of(&cached)["cache_control"],
            json!({"type": "ephemeral"})
        );
    }

    #[test]
    fn tool_choice_writes_each_strategy() {
        assert_eq!(json_of(&ToolChoice::auto()), json!({"type": "auto"}));
        assert_eq!(json_of(&ToolChoice::none()), json!({"type": "none"}));
        assert_eq!(
            json_of(&ToolChoice::Any {
                disable_parallel_tool_use: Some(true)
            }),
            json!({"type": "any", "disable_parallel_tool_use": true})
        );
        assert_eq!(
            json_of(&ToolChoice::Tool {
                name: "Read".into(),
                disable_parallel_tool_use: None,
            }),
            json!({"type": "tool", "name": "Read"})
        );
    }

    // ----------------------------------------------------- thinking and effort

    #[test]
    fn thinking_writes_each_mode() {
        assert_eq!(
            json_of(&ThinkingConfig::adaptive()),
            json!({"type": "adaptive"})
        );
        assert_eq!(
            json_of(&ThinkingConfig::adaptive().summarized()),
            json!({"type": "adaptive", "display": "summarized"})
        );
        assert_eq!(
            json_of(&ThinkingConfig::disabled()),
            json!({"type": "disabled"})
        );
        assert_eq!(
            json_of(&ThinkingConfig::enabled(10_000)),
            json!({"type": "enabled", "budget_tokens": 10_000})
        );
        assert_eq!(
            json_of(&ThinkingConfig::enabled(10_000).summarized()),
            json!({"type": "enabled", "budget_tokens": 10_000, "display": "summarized"})
        );
        // Disabled has no display to set, and asking for one does not invent a
        // field the wire has no meaning for.
        assert_eq!(
            json_of(&ThinkingConfig::disabled().summarized()),
            json!({"type": "disabled"})
        );
    }

    #[test]
    fn an_omitted_display_is_a_field_the_endpoint_chose_not_a_default_we_assumed() {
        let v = json_of(&ThinkingConfig::adaptive());
        assert!(v.get("display").is_none(), "{v}");
    }

    #[test]
    fn effort_writes_every_level_and_says_its_own_name() {
        for (effort, spelled) in [
            (Effort::Low, "low"),
            (Effort::Medium, "medium"),
            (Effort::High, "high"),
            (Effort::Xhigh, "xhigh"),
            (Effort::Max, "max"),
        ] {
            assert_eq!(json_of(&effort), json!(spelled));
            assert_eq!(effort.to_string(), spelled);
            assert_eq!(Effort::from_name(spelled), Some(effort), "and reads back");
            let request = MessagesRequest::new("m", 1, vec![]).with_effort(effort);
            assert_eq!(
                json_of(&request)["output_config"],
                json!({"effort": spelled})
            );
        }
        // A name this crate does not know is not a level to send: an endpoint
        // that serves a tier the spec has no word for says so in its own list,
        // and guessing here would be a 400.
        assert_eq!(Effort::from_name("medium-high"), None);
        assert_eq!(Effort::from_name("off"), None);
    }

    #[test]
    fn cache_control_writes_both_lifetimes() {
        assert_eq!(
            json_of(&CacheControl::ephemeral()),
            json!({"type": "ephemeral"})
        );
        assert_eq!(
            json_of(&CacheControl::ephemeral_hour()),
            json!({"type": "ephemeral", "ttl": "1h"})
        );
        assert_eq!(json_of(&Ttl::FiveMinutes), json!("5m"));
    }

    // --------------------------------------------------------------- response

    #[test]
    fn a_message_parses_with_fields_this_crate_does_not_model() {
        // The spec reserves the right to add to an object; a reader that fails
        // on the addition is a reader that breaks on the endpoint's release.
        let message: Message = serde_json::from_str(
            r#"{
                "id": "msg_1",
                "type": "message",
                "role": "assistant",
                "model": "claude-opus-5",
                "content": [{"type": "text", "text": "hi", "citations": null}],
                "stop_reason": "end_turn",
                "stop_sequence": null,
                "container": {"id": "c1"},
                "usage": {"input_tokens": 5, "output_tokens": 2}
            }"#,
        )
        .expect("an answer parses");
        assert_eq!(message.id, "msg_1");
        assert_eq!(message.model, "claude-opus-5");
        assert_eq!(message.content, vec![Block::text("hi")]);
        assert_eq!(message.stop_reason, Some(StopReason::EndTurn));
        assert_eq!(message.stop_details, None, "absent is absent");
        assert_eq!(message.usage.input_tokens, Some(5));
        assert_eq!(message.usage.output_tokens, Some(2));
    }

    #[test]
    fn a_response_block_kind_this_crate_does_not_model_does_not_fail_the_message() {
        let message: Message = serde_json::from_str(
            r#"{"id":"m","content":[
                {"type":"server_tool_use","id":"s1","name":"web_search","input":{}},
                {"type":"text","text":"after"}
            ]}"#,
        )
        .expect("a message with an unknown block still parses");
        assert_eq!(
            message.content,
            vec![
                Block {
                    kind: BlockKind::Unknown,
                    cache_control: None,
                },
                Block::text("after"),
            ]
        );
    }

    #[test]
    fn an_unfinished_tool_call_reads_as_an_empty_object_rather_than_null() {
        // A block that declared its call and no arguments yet. Indexing a null
        // panics; an empty object is what the wire means by the absence.
        let event = parse_event(
            r#"{"type":"content_block_start","index":1,
                "content_block":{"type":"tool_use","id":"toolu_1","name":"Read"}}"#,
        );
        let Event::ContentBlockStart { content_block, .. } = event else {
            panic!("expected a block start");
        };
        match content_block.kind {
            BlockKind::ToolUse { input, .. } => {
                assert_eq!(input, json!({}));
                assert_eq!(input["file_path"], json!(null), "and indexing it is safe");
            }
            other => panic!("expected a tool call, got {other:?}"),
        }
    }

    #[test]
    fn a_message_that_carries_nothing_optional_still_parses() {
        let message: Message = serde_json::from_str(r#"{"id":"m"}"#).expect("the minimum");
        assert_eq!(message.model, "");
        assert!(message.content.is_empty());
        assert_eq!(message.stop_reason, None);
        assert_eq!(message.usage, Usage::default());
    }

    #[test]
    fn every_stop_reason_is_named_and_an_unknown_one_is_not_a_failure() {
        for (spelled, reason) in [
            ("end_turn", StopReason::EndTurn),
            ("max_tokens", StopReason::MaxTokens),
            ("stop_sequence", StopReason::StopSequence),
            ("tool_use", StopReason::ToolUse),
            ("pause_turn", StopReason::PauseTurn),
            ("refusal", StopReason::Refusal),
            (
                "model_context_window_exceeded",
                StopReason::ModelContextWindowExceeded,
            ),
        ] {
            let parsed: StopReason = serde_json::from_str(&format!("\"{spelled}\"")).unwrap();
            assert_eq!(parsed, reason);
            assert_eq!(reason.as_str(), spelled);
            assert_eq!(reason.to_string(), spelled);
        }
        let future: StopReason = serde_json::from_str("\"invented_later\"").unwrap();
        assert_eq!(future, StopReason::Other);
        assert_eq!(future.to_string(), "other");
    }

    // ------------------------------------------------------------------ usage

    #[test]
    fn usage_tells_absent_from_zero() {
        let usage: Usage =
            serde_json::from_str(r#"{"input_tokens":0,"service_tier":"standard"}"#).unwrap();
        assert_eq!(usage.input_tokens, Some(0), "a reported zero is a value");
        assert_eq!(usage.output_tokens, None, "an absent count is not");
        assert_eq!(usage.service_tier.as_deref(), Some("standard"));
    }

    #[test]
    fn merging_takes_what_the_later_report_carries_and_keeps_the_rest() {
        // The shape the standard endpoint streams: the prompt in
        // message_start, the answer count in message_delta.
        let mut total: Usage = serde_json::from_str(
            r#"{"input_tokens":100,"cache_read_input_tokens":80,"output_tokens":1}"#,
        )
        .unwrap();
        let later: Usage = serde_json::from_str(r#"{"output_tokens":42}"#).unwrap();
        total.merge(&later);
        assert_eq!(total.input_tokens, Some(100), "the earlier prompt is kept");
        assert_eq!(total.output_tokens, Some(42), "the later count wins");
    }

    #[test]
    fn merging_a_reported_zero_over_a_real_count_takes_the_zero() {
        // The quirk this rule exists for, from the other side: an endpoint that
        // opens with zeros must be overwritten by the later truth, not the
        // other way round.
        let mut total: Usage =
            serde_json::from_str(r#"{"input_tokens":0,"output_tokens":0}"#).unwrap();
        let later: Usage =
            serde_json::from_str(r#"{"input_tokens":36,"output_tokens":12}"#).unwrap();
        total.merge(&later);
        assert_eq!(total.input_tokens, Some(36));
        assert_eq!(total.output_tokens, Some(12));
    }

    #[test]
    fn usage_says_which_capacity_and_region_served_the_answer() {
        let usage: Usage = serde_json::from_str(
            r#"{"input_tokens":1,"output_tokens":2,"service_tier":"priority",
                "inference_geo":"us","cache_creation":{"ephemeral_1h_input_tokens":7}}"#,
        )
        .unwrap();
        assert_eq!(usage.service_tier.as_deref(), Some("priority"));
        assert_eq!(usage.inference_geo.as_deref(), Some("us"));
        assert_eq!(
            usage.cache_creation.map(|c| c.ephemeral_1h_input_tokens),
            Some(7)
        );
        // A later event that says nothing about the region keeps what the
        // earlier one said, like every other count.
        let mut merged = usage.clone();
        merged.merge(&Usage::default());
        assert_eq!(merged.inference_geo.as_deref(), Some("us"));
        merged.merge(&Usage {
            inference_geo: Some("eu".into()),
            ..Usage::default()
        });
        assert_eq!(merged.inference_geo.as_deref(), Some("eu"));
    }

    #[test]
    fn the_derived_counts_add_up() {
        let usage: Usage = serde_json::from_str(
            r#"{"input_tokens":36,"cache_read_input_tokens":128,
                "cache_creation_input_tokens":20,"output_tokens":12,
                "output_tokens_details":{"thinking_tokens":7}}"#,
        )
        .unwrap();
        assert_eq!(usage.prompt_tokens(), 184);
        assert_eq!(usage.total_tokens(), 196);
        assert_eq!(usage.thinking_tokens(), Some(7));
        assert_eq!(usage.cache(), Some((128, 20)));
    }

    #[test]
    fn no_cache_figures_is_not_the_same_as_zero_of_them() {
        let none: Usage = serde_json::from_str(r#"{"input_tokens":10}"#).unwrap();
        assert_eq!(none.cache(), None);
        // A reported zero is a fact about the turn: nothing was cached.
        let zero: Usage = serde_json::from_str(r#"{"cache_read_input_tokens":0}"#).unwrap();
        assert_eq!(zero.cache(), Some((0, 0)));
        let plain = Usage::default();
        assert_eq!(plain.cache(), None);
        assert_eq!(plain.thinking_tokens(), None);
        assert_eq!(plain.prompt_tokens(), 0);
        assert_eq!(plain.total_tokens(), 0);
    }

    // ----------------------------------------------------------------- events

    #[test]
    fn the_event_stream_parses_event_by_event() {
        assert_eq!(
            parse_event(
                r#"{"type":"message_start","message":{"id":"m1","model":"MiniMax-M3",
                    "content":[],"usage":{"input_tokens":100,"output_tokens":1}}}"#
            ),
            Event::MessageStart {
                message: Message {
                    id: "m1".into(),
                    model: "MiniMax-M3".into(),
                    content: vec![],
                    stop_reason: None,
                    stop_sequence: None,
                    stop_details: None,
                    usage: Usage {
                        input_tokens: Some(100),
                        output_tokens: Some(1),
                        ..Usage::default()
                    },
                },
            }
        );
        assert_eq!(
            parse_event(
                r#"{"type":"content_block_start","index":0,
                    "content_block":{"type":"thinking","thinking":""}}"#
            ),
            Event::ContentBlockStart {
                index: 0,
                content_block: Block::thinking("", ""),
            }
        );
        assert_eq!(
            parse_event(r#"{"type":"content_block_stop","index":2}"#),
            Event::ContentBlockStop { index: 2 }
        );
        assert_eq!(
            parse_event(r#"{"type":"message_stop"}"#),
            Event::MessageStop
        );
        assert_eq!(parse_event(r#"{"type":"ping"}"#), Event::Ping);
        assert_eq!(
            parse_event(
                r#"{"type":"message_delta","delta":{"stop_reason":"tool_use",
                    "stop_sequence":null,"stop_details":null},
                    "usage":{"output_tokens":89}}"#
            ),
            Event::MessageDelta {
                delta: MessageDeltaBody {
                    stop_reason: Some(StopReason::ToolUse),
                    stop_sequence: None,
                    stop_details: None,
                },
                usage: Some(Usage {
                    output_tokens: Some(89),
                    ..Usage::default()
                }),
            }
        );
        assert_eq!(
            parse_event(
                r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#
            ),
            Event::Error {
                error: crate::StreamError {
                    r#type: "overloaded_error".into(),
                    message: "Overloaded".into(),
                },
            }
        );
    }

    #[test]
    fn every_delta_kind_parses_and_an_unknown_one_does_not_fail_the_stream() {
        let delta = |line: &str| match parse_event(line) {
            Event::ContentBlockDelta { delta, .. } => delta,
            other => panic!("expected a delta, got {other:?}"),
        };
        assert_eq!(
            delta(
                r#"{"type":"content_block_delta","index":0,
                "delta":{"type":"text_delta","text":"hi"}}"#
            ),
            BlockDelta::TextDelta { text: "hi".into() }
        );
        assert_eq!(
            delta(
                r#"{"type":"content_block_delta","index":0,
                "delta":{"type":"thinking_delta","thinking":"hmm"}}"#
            ),
            BlockDelta::ThinkingDelta {
                thinking: "hmm".into()
            }
        );
        assert_eq!(
            delta(
                r#"{"type":"content_block_delta","index":0,
                "delta":{"type":"signature_delta","signature":"sig"}}"#
            ),
            BlockDelta::SignatureDelta {
                signature: "sig".into()
            }
        );
        assert_eq!(
            delta(
                r#"{"type":"content_block_delta","index":0,
                "delta":{"type":"input_json_delta","partial_json":"{\"a\""}}"#
            ),
            BlockDelta::InputJsonDelta {
                partial_json: "{\"a\"".into()
            }
        );
        assert_eq!(
            delta(
                r#"{"type":"content_block_delta","index":0,
                "delta":{"type":"citations_delta","citation":{"url":"x"}}}"#
            ),
            BlockDelta::CitationsDelta {
                citation: json!({"url": "x"})
            }
        );
        assert_eq!(
            delta(
                r#"{"type":"content_block_delta","index":0,
                "delta":{"type":"invented_later","payload":1}}"#
            ),
            BlockDelta::Unknown
        );
    }

    #[test]
    fn an_event_type_this_crate_does_not_model_is_carried_not_refused() {
        // The spec adds event types and tells clients to handle the unknown
        // ones gracefully; a stream that ends because it met a new event type
        // is a stream that ends on the endpoint's release day.
        assert_eq!(
            parse_event(r#"{"type":"future_event","x":1}"#),
            Event::Unknown
        );
    }
}
