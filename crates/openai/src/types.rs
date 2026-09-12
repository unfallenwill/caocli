//! The Chat Completions API as types.
//!
//! Two halves that never meet. The **request** half is serialized and never
//! parsed: its field names are the API's field names, so a glance at a type is
//! a glance at the JSON. The **response** half is parsed and never serialized,
//! and it is tolerant by construction — the spec says enum values may be added,
//! so every enum has a catch-all and unknown object fields are ignored rather
//! than refused.
//!
//! What is here is the published Chat Completions API: the request body field
//! for field, the answer's choices, the streaming chunks, the usage counts, the
//! message and content-part variants, the tool definitions and the tool calls
//! the model may emit. What is deliberately not here at all: audio output,
//! web search, predicted outputs, prompt caching, moderation, the legacy
//! `functions` / `function_call` fields (deprecated in favor of
//! `tools` / `tool_choice`), the `Responses` API, the Assistants API, the
//! batch endpoints, the files API, and every other product surface. Those are
//! either a different endpoint or a different product; a call this crate
//! cannot make is a type nobody can check.
//!
//! # Shape
//!
//! ```
//! use caocli_openai::{
//!     ChatCompletionRequest, ChatCompletionMessageParam, FunctionDefinition,
//!     Tool, UserMessageParam,
//! };
//!
//! let request = ChatCompletionRequest::new(
//!     "gpt-4o",
//!     vec![ChatCompletionMessageParam::User(
//!         UserMessageParam::new("say hello"),
//!     )],
//! ).into_streaming()
//!  .with_max_tokens(1024)
//!  .with_tool(Tool::function(FunctionDefinition::new(
//!      "get_weather",
//!      serde_json::json!({
//!          "type": "object",
//!          "properties": {"city": {"type": "string"}},
//!      }),
//!  )));
//!
//! assert!(request.stream);
//! ```

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ============================================================================
// Request: messages and content
// ============================================================================

/// The role of a message on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// `system` — top-level instructions. The `developer` role is the modern
    /// equivalent and what newer models expect; the spec still accepts both.
    System,
    /// `developer` — instructions introduced with the o-series models. Sent
    /// with higher priority than `system` on the backends that distinguish.
    Developer,
    /// `user` — what the end user said.
    User,
    /// `assistant` — what the model said. Used both for prior turns in a
    /// conversation and for tool calls.
    Assistant,
    /// `tool` — the result of a tool call, addressed by `tool_call_id`.
    Tool,
    /// `function` — deprecated; kept for callers that still carry one.
    Function,
}

/// The content of a message: a plain string, or the array of parts that
/// carries images. Untagged, and the string tried first, so a stored text
/// message is read and written back as the very string it was.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    /// A plain string, with no parts form.
    Text(String),
    /// The parts form, when the message carries images.
    Parts(Vec<ContentPart>),
}

impl MessageContent {
    /// A content carrying the given text only.
    pub fn text(text: impl Into<String>) -> Self {
        MessageContent::Text(text.into())
    }

    /// A content carrying the given parts.
    pub fn parts(parts: Vec<ContentPart>) -> Self {
        MessageContent::Parts(parts)
    }

    /// The text the content carries, whichever shape it is in.
    pub fn as_text(&self) -> String {
        match self {
            MessageContent::Text(text) => text.clone(),
            MessageContent::Parts(parts) => parts
                .iter()
                .filter_map(|p| p.text_value())
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

impl From<&str> for MessageContent {
    fn from(text: &str) -> Self {
        MessageContent::Text(text.to_owned())
    }
}

impl From<String> for MessageContent {
    fn from(text: String) -> Self {
        MessageContent::Text(text)
    }
}

/// One part of a message's content: a run of text, or an image.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    /// A run of text. The simple form.
    #[serde(rename = "text")]
    Text {
        /// The text itself.
        text: String,
    },
    /// An image, addressed by URL or by a base64 `data:` URL.
    #[serde(rename = "image_url")]
    ImageUrl {
        /// The image's source: a regular URL, or `data:image/...;base64,...`.
        image_url: ImageUrl,
    },
}

impl ContentPart {
    /// A text part with the given text.
    pub fn text(text: impl Into<String>) -> Self {
        ContentPart::Text { text: text.into() }
    }

    /// An image part with the given URL.
    pub fn image_url(url: impl Into<String>) -> Self {
        ContentPart::ImageUrl {
            image_url: ImageUrl {
                url: url.into(),
                detail: None,
            },
        }
    }

    /// The text this part carries, when it is a text part. None when it is an
    /// image, because the bytes are not text.
    pub fn text_value(&self) -> Option<&str> {
        match self {
            ContentPart::Text { text } => Some(text.as_str()),
            ContentPart::ImageUrl { .. } => None,
        }
    }
}

/// Where an image part reads its image, and how much of it to look at.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageUrl {
    /// Either a URL of the image, or the base64-encoded image data (`data:` URL).
    pub url: String,
    /// How much of the image to spend tokens on. The endpoint's default is `auto`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub detail: Option<ImageDetail>,
}

/// The detail level for image parts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageDetail {
    /// The model's own pick.
    Auto,
    /// Spend fewer tokens — enough for a quick glance.
    Low,
    /// Spend more tokens — pixel-level resolution.
    High,
}

/// A message sent by the developer. Model-agnostic instructions, sent with
/// the highest priority on the backends that distinguish.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeveloperMessageParam {
    /// The contents of the developer message.
    pub content: MessageContent,
    /// An optional name for the participant, distinguishing them from others
    /// of the same role.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub name: Option<String>,
}

impl DeveloperMessageParam {
    /// A developer message with the given text.
    pub fn new(content: impl Into<MessageContent>) -> Self {
        Self {
            content: content.into(),
            name: None,
        }
    }

    /// Name this participant.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }
}

/// A system message: instructions that apply across the whole conversation.
/// Newer models expect [`DeveloperMessageParam`] instead.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemMessageParam {
    /// The contents of the system message.
    pub content: MessageContent,
    /// An optional name for the participant, distinguishing them from others
    /// of the same role.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub name: Option<String>,
}

impl SystemMessageParam {
    /// A system message with the given text.
    pub fn new(content: impl Into<MessageContent>) -> Self {
        Self {
            content: content.into(),
            name: None,
        }
    }

    /// Name this participant.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }
}

/// A message sent by the end user.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserMessageParam {
    /// The contents of the user message.
    pub content: MessageContent,
    /// An optional name for the participant, distinguishing them from others
    /// of the same role.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub name: Option<String>,
}

impl UserMessageParam {
    /// A user message with the given text.
    pub fn new(content: impl Into<MessageContent>) -> Self {
        Self {
            content: content.into(),
            name: None,
        }
    }

    /// A user message carrying the given text and the given image URLs.
    pub fn with_images(text: impl Into<String>, image_urls: Vec<String>) -> Self {
        let text = text.into();
        let mut parts = Vec::with_capacity(image_urls.len() + 1);
        if !text.is_empty() {
            parts.push(ContentPart::text(text));
        }
        parts.extend(image_urls.into_iter().map(ContentPart::image_url));
        Self {
            content: MessageContent::Parts(parts),
            name: None,
        }
    }

    /// Name this participant.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }
}

/// The function-call payload of an assistant message from a model that used
/// the deprecated `function_call` field. Most callers will never construct one
/// — [`AssistantMessageParam`] with `tool_calls` is the modern shape — but a
/// model that still uses the old field sends one back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionCall {
    /// The name of the function the model called.
    pub name: String,
    /// The arguments to call it with, as a JSON string the model generated.
    pub arguments: String,
}

/// An assistant message: what the model said, or the tool calls it made.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessageParam {
    /// The text of the assistant's turn. Required unless `tool_calls` or the
    /// deprecated `function_call` carries the whole answer.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub content: Option<MessageContent>,
    /// The tool calls the model made. Empty or absent on a pure-text turn.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tool_calls: Option<Vec<MessageToolCall>>,
    /// The deprecated function-call form. Most callers will never set it.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub function_call: Option<FunctionCall>,
    /// The refusal message by the assistant, when it refused.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub refusal: Option<String>,
    /// An optional name for the participant, distinguishing them from others
    /// of the same role.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub name: Option<String>,
    /// Extra fields to ship on this message — vendor extensions the SDK does
    /// not name. The same asymmetric contract as
    /// [`ChatCompletionRequest::extra_body`]: a key here must not also be a
    /// typed field (serde panics on duplicates at serialize time); at
    /// deserialize time the typed fields take priority, and a value the type
    /// rejects is a parse error rather than a fallback to this map.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra_body: BTreeMap<String, Value>,
}

impl AssistantMessageParam {
    /// An assistant message with the given text.
    pub fn new(content: impl Into<MessageContent>) -> Self {
        Self {
            content: Some(content.into()),
            tool_calls: None,
            function_call: None,
            refusal: None,
            name: None,
            extra_body: BTreeMap::new(),
        }
    }

    /// An assistant message carrying the given tool calls.
    pub fn with_tool_calls(tool_calls: Vec<MessageToolCall>) -> Self {
        Self {
            content: None,
            tool_calls: Some(tool_calls),
            function_call: None,
            refusal: None,
            name: None,
            extra_body: BTreeMap::new(),
        }
    }

    /// Name this participant.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Add one entry to [`Self::extra_body`]. See that field for the
    /// contract: a key here must not also be a typed field on this struct.
    pub fn with_extra_body(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.extra_body.insert(key.into(), value.into());
        self
    }
}

/// A message carrying the result of a tool call, addressed by `tool_call_id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolMessageParam {
    /// The contents of the tool message.
    pub content: MessageContent,
    /// The id of the tool call this message is responding to.
    pub tool_call_id: String,
}

impl ToolMessageParam {
    /// A tool result with the given text, responding to the given call.
    pub fn new(tool_call_id: impl Into<String>, content: impl Into<MessageContent>) -> Self {
        Self {
            content: content.into(),
            tool_call_id: tool_call_id.into(),
        }
    }
}

/// A deprecated function-result message. Most callers will never construct one
/// — [`ToolMessageParam`] with `tool_call_id` is the modern shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionMessageParam {
    /// The contents of the function message.
    pub content: MessageContent,
    /// The name of the function whose result this is.
    pub name: String,
}

impl FunctionMessageParam {
    /// A function-result message with the given text.
    pub fn new(name: impl Into<String>, content: impl Into<MessageContent>) -> Self {
        Self {
            content: content.into(),
            name: name.into(),
        }
    }
}

/// A message in the request. Tagged by `role`, so a user message stays a user
/// message and a tool result stays a tool result through a round-trip.
///
/// The constructor helpers — [`ChatCompletionMessageParam::user`],
/// [`Self::system`], [`Self::assistant`], [`Self::developer`],
/// [`Self::tool`] — name the variant by what it is, so a call site reads as
/// "a user message" rather than as the variant's struct name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum ChatCompletionMessageParam {
    /// A developer message.
    Developer(DeveloperMessageParam),
    /// A system message.
    System(SystemMessageParam),
    /// A user message.
    User(UserMessageParam),
    /// An assistant message.
    Assistant(AssistantMessageParam),
    /// A tool result.
    Tool(ToolMessageParam),
    /// A deprecated function result.
    Function(FunctionMessageParam),
}

impl ChatCompletionMessageParam {
    /// A user message with the given text. Convenience for the common case.
    pub fn user(content: impl Into<MessageContent>) -> Self {
        ChatCompletionMessageParam::User(UserMessageParam::new(content))
    }

    /// A system message with the given text.
    pub fn system(content: impl Into<MessageContent>) -> Self {
        ChatCompletionMessageParam::System(SystemMessageParam::new(content))
    }

    /// A developer message with the given text.
    pub fn developer(content: impl Into<MessageContent>) -> Self {
        ChatCompletionMessageParam::Developer(DeveloperMessageParam::new(content))
    }

    /// An assistant message with the given text.
    pub fn assistant(content: impl Into<MessageContent>) -> Self {
        ChatCompletionMessageParam::Assistant(AssistantMessageParam::new(content))
    }

    /// An assistant message carrying the given tool calls.
    pub fn assistant_tool_calls(tool_calls: Vec<MessageToolCall>) -> Self {
        ChatCompletionMessageParam::Assistant(AssistantMessageParam::with_tool_calls(tool_calls))
    }

    /// A tool result for the given call.
    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<MessageContent>) -> Self {
        ChatCompletionMessageParam::Tool(ToolMessageParam::new(tool_call_id, content))
    }
}

// ============================================================================
// Request: tools, tool choice, response format
// ============================================================================

/// The function a tool definition describes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionDefinition {
    /// The name of the function. Unique within a request.
    pub name: String,
    /// A description of what the function does. The model uses this to decide
    /// when to call it.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub description: Option<String>,
    /// The parameters the function accepts, as a JSON Schema object.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub parameters: Option<Value>,
    /// Whether the schema is enforced strictly. Supported on most modern models.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub strict: Option<bool>,
}

impl FunctionDefinition {
    /// A function definition with the given name and parameter schema.
    pub fn new(name: impl Into<String>, parameters: Value) -> Self {
        Self {
            name: name.into(),
            description: None,
            parameters: Some(parameters),
            strict: None,
        }
    }

    /// Describe what the function does. The model uses this to decide when to
    /// call it.
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Require the model to produce arguments that match the schema exactly.
    pub fn with_strict(mut self, strict: bool) -> Self {
        self.strict = Some(strict);
        self
    }
}

/// A function tool: the model calls a function you defined.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionTool {
    /// The kind of tool, always `"function"`.
    #[serde(rename = "type")]
    pub r#type: ToolType,
    /// The function the model may call.
    pub function: FunctionDefinition,
}

impl FunctionTool {
    /// A function tool wrapping the given function definition.
    pub fn new(function: FunctionDefinition) -> Self {
        Self {
            r#type: ToolType::Function,
            function,
        }
    }
}

/// The kind of a tool: only `function` is widely supported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolType {
    /// A function the model calls with JSON arguments.
    Function,
}

/// A tool the model may call. The spec allows more kinds; only `Function` is
/// modeled here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Tool {
    /// A function tool.
    Function(FunctionTool),
}

impl Tool {
    /// A function tool wrapping the given function definition.
    pub fn function(function: FunctionDefinition) -> Self {
        Tool::Function(FunctionTool::new(function))
    }
}

/// A specific function the model is forced to call, named in `function.name`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NamedToolChoice {
    /// The kind of tool, always `"function"`.
    #[serde(rename = "type")]
    pub r#type: ToolType,
    /// The function the model should call.
    pub function: NamedFunction,
}

impl NamedToolChoice {
    /// A choice that forces the model to call the named function.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            r#type: ToolType::Function,
            function: NamedFunction { name: name.into() },
        }
    }
}

/// The function a [`NamedToolChoice`] picks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NamedFunction {
    /// The name of the function the model should call.
    pub name: String,
}

/// How the model should pick a tool (or not).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolChoice {
    /// `"none"` — the model will not call any tool.
    /// `"auto"` — the model may call or answer. `"required"` — the model must call.
    Mode(ToolChoiceMode),
    /// Force the model to call a specific function.
    Named(NamedToolChoice),
    /// Constrain the model to a pre-defined set of tools. Use
    /// [`ToolChoice::allowed_required`] to force it to call one.
    Allowed(ChatCompletionAllowedToolChoice),
    /// Force the model to call a specific custom tool.
    NamedCustom(ChatCompletionNamedToolChoiceCustom),
}

impl ToolChoice {
    /// `"auto"` — the model may call or answer.
    pub fn auto() -> Self {
        ToolChoice::Mode(ToolChoiceMode::Auto)
    }

    /// `"none"` — the model will not call any tool.
    pub fn none() -> Self {
        ToolChoice::Mode(ToolChoiceMode::None)
    }

    /// `"required"` — the model must call one of the tools.
    pub fn required() -> Self {
        ToolChoice::Mode(ToolChoiceMode::Required)
    }

    /// Force the model to call the named function.
    pub fn function(name: impl Into<String>) -> Self {
        ToolChoice::Named(NamedToolChoice::new(name))
    }

    /// Constrain the model to a pre-defined set of tools.
    pub fn allowed(
        tools: Vec<ChatCompletionAllowedToolRef>,
        mode: ChatCompletionAllowedToolMode,
    ) -> Self {
        ToolChoice::Allowed(ChatCompletionAllowedToolChoice {
            allowed_tools: ChatCompletionAllowedTools { mode, tools },
            r#type: AllowedToolsDiscriminator::AllowedTools,
        })
    }

    /// Force the model to call the named custom tool.
    pub fn custom(name: impl Into<String>) -> Self {
        ToolChoice::NamedCustom(ChatCompletionNamedToolChoiceCustom {
            custom: ChatCompletionNamedToolChoiceCustomInner { name: name.into() },
            r#type: CustomToolDiscriminator::Custom,
        })
    }
}

/// The three string forms of [`ToolChoice`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolChoiceMode {
    /// The model may call a tool or answer in text.
    Auto,
    /// The model must not call a tool.
    None,
    /// The model must call one of the tools.
    Required,
}

/// The mode for [`ToolChoice::allowed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatCompletionAllowedToolMode {
    /// The model picks from the allowed tools or generates a message.
    Auto,
    /// The model must call one or more of the allowed tools.
    Required,
}

/// A reference to a tool in a [`ChatCompletionAllowedToolChoice::allowed_tools`]
/// list. Modeled as the same shape used by the wire: `{"type":..., "function":{...}}`
/// or `{"type":..., "name":...}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatCompletionAllowedToolRef {
    /// A function tool.
    #[serde(rename = "function")]
    Function {
        /// The function the model may call. Chat Completions nests the
        /// function fields under a `function` key (unlike Responses).
        #[serde(flatten)]
        function: AllowedFunctionFields,
    },
    /// A custom tool.
    #[serde(rename = "custom")]
    Custom {
        /// The tool's name.
        name: String,
    },
}

/// The function-name field on a [`ChatCompletionAllowedToolRef::Function`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AllowedFunctionFields {
    /// The function's name.
    pub name: String,
}

/// The set of tools the model is allowed to call, on Chat Completions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatCompletionAllowedTools {
    /// Whether the model may or must call one of the allowed tools.
    pub mode: ChatCompletionAllowedToolMode,
    /// The tools the model is allowed to call.
    pub tools: Vec<ChatCompletionAllowedToolRef>,
}

/// Constrain the model to a pre-defined set of tools on Chat Completions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatCompletionAllowedToolChoice {
    /// The mode and list of allowed tools.
    pub allowed_tools: ChatCompletionAllowedTools,
    /// The kind of tool choice, always `"allowed_tools"`.
    #[serde(rename = "type")]
    pub r#type: AllowedToolsDiscriminator,
}

/// The tag for [`ChatCompletionAllowedToolChoice`], always `"allowed_tools"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AllowedToolsDiscriminator {
    /// The model picks from the allowed tools.
    #[serde(rename = "allowed_tools")]
    AllowedTools,
}

/// Force the model to call a specific custom tool on Chat Completions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatCompletionNamedToolChoiceCustom {
    /// The custom tool the model should call.
    pub custom: ChatCompletionNamedToolChoiceCustomInner,
    /// The kind of tool choice, always `"custom"`.
    #[serde(rename = "type")]
    pub r#type: CustomToolDiscriminator,
}

/// The custom tool the model should call: just its name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatCompletionNamedToolChoiceCustomInner {
    /// The name of the custom tool to call.
    pub name: String,
}

/// The tag for [`ChatCompletionNamedToolChoiceCustom`], always `"custom"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CustomToolDiscriminator {
    /// The custom tool choice.
    #[serde(rename = "custom")]
    Custom,
}

/// The shape of the model's answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseFormat {
    /// Plain text. The default.
    #[serde(rename = "text")]
    Text,
    /// A JSON object. The model is steered toward producing valid JSON.
    #[serde(rename = "json_object")]
    JsonObject,
    /// A JSON object matching the supplied schema.
    #[serde(rename = "json_schema")]
    JsonSchema {
        /// The schema the model's output must match.
        json_schema: JsonSchemaSpec,
    },
}

impl ResponseFormat {
    /// Plain text.
    pub fn text() -> Self {
        ResponseFormat::Text
    }

    /// A JSON object, no schema.
    pub fn json_object() -> Self {
        ResponseFormat::JsonObject
    }

    /// A JSON object matching the given schema.
    pub fn json_schema(spec: JsonSchemaSpec) -> Self {
        ResponseFormat::JsonSchema { json_schema: spec }
    }
}

/// The schema definition a [`ResponseFormat::JsonSchema`] enforces.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonSchemaSpec {
    /// The schema name, used in error messages and for naming the structured
    /// output in tool-call style.
    pub name: String,
    /// The schema itself, as a JSON Schema object.
    pub schema: Value,
    /// Whether the model's output is enforced strictly.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub strict: Option<bool>,
    /// A description of what the schema represents.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub description: Option<String>,
}

impl JsonSchemaSpec {
    /// A schema spec with the given name and schema body.
    pub fn new(name: impl Into<String>, schema: Value) -> Self {
        Self {
            name: name.into(),
            schema,
            strict: None,
            description: None,
        }
    }

    /// Require the model to produce output that matches the schema exactly.
    pub fn with_strict(mut self, strict: bool) -> Self {
        self.strict = Some(strict);
        self
    }

    /// Describe what the schema represents.
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

// ============================================================================
// Request: top-level knobs
// ============================================================================

/// The output modality the model may produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Modality {
    /// Plain text. The default.
    Text,
    /// Audio output. Requires [`AudioConfig`] to also be set.
    Audio,
}

/// The format an audio answer is encoded in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioFormat {
    /// WAV.
    Wav,
    /// AAC.
    Aac,
    /// MP3.
    Mp3,
    /// FLAC.
    Flac,
    /// Opus.
    Opus,
    /// 16-bit linear PCM.
    Pcm16,
}

/// A voice the model uses to respond: a built-in name or a custom voice id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Voice {
    /// A built-in voice name (e.g. `"alloy"`, `"ash"`).
    BuiltIn(String),
    /// A reference to a custom voice the endpoint has.
    Custom {
        /// The voice id.
        id: String,
    },
}

/// Audio output configuration. Required when `modalities` includes `"audio"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioConfig {
    /// The output audio format.
    pub format: AudioFormat,
    /// The voice the model uses to respond.
    pub voice: Voice,
}

impl AudioConfig {
    /// An audio output configuration with the given format and voice.
    pub fn new(format: AudioFormat, voice: impl Into<String>) -> Self {
        Self {
            format,
            voice: Voice::BuiltIn(voice.into()),
        }
    }

    /// Use a custom voice id rather than a built-in name.
    pub fn with_custom_voice(mut self, id: impl Into<String>) -> Self {
        self.voice = Voice::Custom { id: id.into() };
        self
    }
}

/// The policy the moderation step applies to the response input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModerationPolicyInput {
    /// `score` returns a score; `block` short-circuits the request.
    pub mode: ModerationMode,
}

/// The policy the moderation step applies to the generated output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModerationPolicyOutput {
    /// `score` returns a score; `block` short-circuits the response.
    pub mode: ModerationMode,
}

/// The mode the moderation step runs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModerationMode {
    /// Return the moderation score alongside the answer.
    Score,
    /// Refuse to answer when the input or output trips the policy.
    Block,
}

/// The policy the moderation step applies to input and output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModerationPolicy {
    /// The input policy.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub input: Option<ModerationPolicyInput>,
    /// The output policy.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub output: Option<ModerationPolicyOutput>,
}

/// Configuration for running moderation on the request input and the
/// generated output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModerationConfig {
    /// The moderation model to use, e.g. `"omni-moderation-latest"`.
    pub model: String,
    /// The policy to apply. Defaults to scoring both input and output.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub policy: Option<ModerationPolicy>,
}

impl ModerationConfig {
    /// A moderation config using the named model.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            policy: None,
        }
    }

    /// Set the moderation policy.
    pub fn with_policy(mut self, policy: ModerationPolicy) -> Self {
        self.policy = Some(policy);
        self
    }
}

/// Static content the model is expected to match. Used with predicted outputs
/// to short-circuit generation when the prefix is already known.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PredictionConfig {
    /// The content the model is expected to match.
    pub content: PredictionContent,
    /// The kind of prediction. Always `"content"`.
    #[serde(rename = "type")]
    pub r#type: PredictionType,
}

/// The content the model is expected to match. Either a plain string or a
/// list of text parts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PredictionContent {
    /// Plain text.
    Text(String),
    /// A list of text parts.
    Parts(Vec<ContentPart>),
}

/// The kind of [`PredictionConfig`]. The spec only defines `"content"` today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PredictionType {
    /// The prediction is content the model is expected to match verbatim.
    Content,
}

impl PredictionConfig {
    /// A prediction matching the given text.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: PredictionContent::Text(text.into()),
            r#type: PredictionType::Content,
        }
    }
}

/// Constrains how verbose the model's answer is. Only some models honor it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verbosity {
    /// The shortest possible answer.
    Low,
    /// A balanced amount. The default on models that honor verbosity.
    Medium,
    /// A long, detailed answer.
    High,
}

/// Options for the web search built-in tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WebSearchOptions {
    /// The search context size: how much of a page the search tool sees.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub search_context_size: Option<SearchContextSize>,
    /// The approximate location the search runs from. Affects results'
    /// localness.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub user_location: Option<UserLocation>,
}

impl WebSearchOptions {
    /// Web search options with the default context size.
    pub fn new() -> Self {
        Self {
            search_context_size: None,
            user_location: None,
        }
    }

    /// Set the search context size.
    pub fn with_search_context_size(mut self, size: SearchContextSize) -> Self {
        self.search_context_size = Some(size);
        self
    }

    /// Set the approximate user location.
    pub fn with_user_location(mut self, location: UserLocation) -> Self {
        self.user_location = Some(location);
        self
    }
}

impl Default for WebSearchOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// The search context size: how much of a page the search tool sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchContextSize {
    /// A short snippet per result.
    Low,
    /// A medium excerpt.
    Medium,
    /// The whole page.
    High,
}

/// The approximate location the web search runs from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserLocation {
    /// The country, as a two-letter ISO 3166-1 code.
    pub country: String,
    /// The city.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub city: Option<String>,
    /// The region (state / province).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub region: Option<String>,
    /// The IANA timezone, e.g. `"America/Los_Angeles"`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub timezone: Option<String>,
}

impl UserLocation {
    /// A user location with just the country set.
    pub fn country(country: impl Into<String>) -> Self {
        Self {
            country: country.into(),
            city: None,
            region: None,
            timezone: None,
        }
    }

    /// Set the city.
    pub fn with_city(mut self, city: impl Into<String>) -> Self {
        self.city = Some(city.into());
        self
    }

    /// Set the region (state / province).
    pub fn with_region(mut self, region: impl Into<String>) -> Self {
        self.region = Some(region.into());
        self
    }

    /// Set the IANA timezone, e.g. `"America/Los_Angeles"`.
    pub fn with_timezone(mut self, timezone: impl Into<String>) -> Self {
        self.timezone = Some(timezone.into());
        self
    }
}

/// Options for prompt caching on `gpt-5.6` and later.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromptCacheOptions {
    /// Whether the endpoint creates an implicit cache breakpoint.
    pub mode: PromptCacheMode,
    /// The minimum lifetime applied to each breakpoint.
    pub ttl: PromptCacheTtl,
    /// The id of a response to compare against when diagnosing cache reuse.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub comparison_response_id: Option<String>,
}

/// Whether implicit breakpoints are written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PromptCacheMode {
    /// One implicit breakpoint, plus up to three explicit ones.
    Implicit,
    /// No implicit breakpoint; up to four explicit ones.
    Explicit,
}

/// The minimum lifetime of a cache breakpoint. The spec currently only
/// defines `"30m"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PromptCacheTtl {
    /// Thirty minutes.
    #[serde(rename = "30m")]
    ThirtyMinutes,
}

impl PromptCacheOptions {
    /// A cache-options object with the default mode and the only supported TTL.
    pub fn new() -> Self {
        Self {
            mode: PromptCacheMode::Implicit,
            ttl: PromptCacheTtl::ThirtyMinutes,
            comparison_response_id: None,
        }
    }

    /// Set the mode.
    pub fn with_mode(mut self, mode: PromptCacheMode) -> Self {
        self.mode = mode;
        self
    }

    /// Set the comparison-response id for diagnostics.
    pub fn with_comparison_response_id(mut self, id: impl Into<String>) -> Self {
        self.comparison_response_id = Some(id.into());
        self
    }
}

impl Default for PromptCacheOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// How long the prompt cache should keep its entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PromptCacheRetention {
    /// Default in-memory cache. May be evicted under memory pressure.
    #[serde(rename = "in_memory")]
    InMemory,
    /// Extended 24-hour cache.
    #[serde(rename = "24h")]
    TwentyFourHours,
}

/// The deprecated `function_call` tool-choice form: either `"none"` / `"auto"`
/// or a specific function name. Use [`ToolChoice`] instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FunctionCallOption {
    /// `"none"` — the model will not call any function.
    /// `"auto"` — the model may call a function or answer in text.
    Mode(FunctionCallMode),
    /// Force the model to call the named function.
    Named {
        /// The function's name.
        name: String,
    },
}

impl FunctionCallOption {
    /// `"none"` — the model will not call any function.
    pub fn none() -> Self {
        FunctionCallOption::Mode(FunctionCallMode::None)
    }

    /// `"auto"` — the model may call a function or answer in text.
    pub fn auto() -> Self {
        FunctionCallOption::Mode(FunctionCallMode::Auto)
    }

    /// Force the model to call the named function.
    pub fn named(name: impl Into<String>) -> Self {
        FunctionCallOption::Named { name: name.into() }
    }
}

/// The string forms of [`FunctionCallOption`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FunctionCallMode {
    /// The model will not call any function.
    None,
    /// The model may call a function or answer in text.
    Auto,
}

/// The reasoning effort: how much the model thinks before answering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    /// No reasoning. The fastest, often the weakest on hard problems.
    None,
    /// A token of thinking. The cheapest tier that uses reasoning tokens.
    Minimal,
    /// Light reasoning.
    Low,
    /// A balanced amount.
    Medium,
    /// More thinking.
    High,
    /// The most the model can spend. Not every model supports every tier.
    XHigh,
}

/// The service tier the request should run on. The endpoint's default is `auto`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceTier {
    /// The model's own choice, falling back to the project's default.
    Auto,
    /// Standard pricing and performance.
    Default,
    /// Lower priority, cheaper when the project is configured for it.
    Flex,
    /// Higher throughput at the cost of consistency.
    Scale,
    /// Reserved capacity, higher priority.
    Priority,
}

/// The stop sequences: the model's answer ends when one of these is produced.
/// Either a single string or a list of strings, both on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Stop {
    /// One stop sequence.
    Single(String),
    /// Up to four stop sequences.
    Many(Vec<String>),
}

/// Options for the streaming response. Only meaningful when `stream: true`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StreamOptions {
    /// Whether to include the final usage chunk. The reference default is `false`,
    /// so without this the `usage` field on a chunk is `null`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_usage: Option<bool>,
    /// Whether to include the obfuscation string on delta chunks. The reference
    /// default is `true`; set to `false` to skip it for bandwidth when the
    /// network is trusted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_obfuscation: Option<bool>,
}

impl StreamOptions {
    /// A stream options object that asks for the final usage chunk.
    pub fn with_usage() -> Self {
        Self {
            include_usage: Some(true),
            include_obfuscation: None,
        }
    }

    /// A stream options object that asks for obfuscation to be turned off.
    pub fn without_obfuscation() -> Self {
        Self {
            include_usage: None,
            include_obfuscation: Some(false),
        }
    }
}

/// Set of key-value pairs the backend can attach to an object. Used to label
/// requests for the dashboard and for later retrieval.
pub type Metadata = HashMap<String, String>;

/// The `POST /v1/chat/completions` body.
///
/// Built rather than assembled: `new` takes the two fields the spec requires,
/// and every other field has a `with_…` that names what it does on the wire, so
/// a request cannot carry a field nobody meant to send. Nothing here is a
/// default: an omitted field is absent from the JSON, not null, because the
/// two are not the same request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatCompletionRequest {
    /// The model id. Spec-required.
    pub model: String,
    /// The conversation, oldest first. Spec-required.
    pub messages: Vec<ChatCompletionMessageParam>,

    /// The maximum number of tokens the model may produce in this turn.
    /// Deprecated in favor of [`Self::max_completion_tokens`] on newer models.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// An upper bound on the tokens the model produces, including reasoning
    /// tokens. The replacement for `max_tokens` on o-series and newer models.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u32>,

    /// Sampling temperature, between 0 and 2.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Nucleus sampling: keep the top tokens whose cumulative probability is
    /// at most `top_p`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,

    /// How many choices to generate for the same prompt. Costs scale with `n`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub n: Option<u32>,

    /// Up to four sequences that, when produced, end the answer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Stop>,

    /// Positive values penalize tokens that have already appeared, lowering
    /// the chance the model repeats itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f64>,
    /// Positive values penalize tokens that have appeared frequently, lowering
    /// the chance the model repeats the same line verbatim.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f64>,

    /// Per-token logit bias: token id → bias, in the range -100 to 100.
    /// A bias of -100 bans the token; +100 forces it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logit_bias: Option<HashMap<String, i32>>,

    /// Whether to return log probabilities of the output tokens.
    /// When `true`, [`Self::top_logprobs`] controls how many of the top
    /// alternatives come back.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<bool>,
    /// The number of top alternative tokens to return per position. Requires
    /// [`Self::logprobs`] to be `true`. Bounded 0..=20.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_logprobs: Option<u32>,

    /// A seed for the sampler. With the same seed and same parameters, the
    /// same model will produce the same output. The model's own determinism
    /// is best-effort.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,

    /// Whether to store the response for later retrieval via the API.
    /// Required to be `true` for some evals / fine-tuning flows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,

    /// A stable identifier for the end user making the request. Used for
    /// abuse detection. Deprecated in favor of [`Self::safety_identifier`]
    /// and [`Self::prompt_cache_key`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,

    /// A stable identifier that helps the endpoint detect users violating
    /// the usage policies. Preferred to [`Self::user`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub safety_identifier: Option<String>,

    /// A key the endpoint uses to bucket requests for prompt caching.
    /// Replaces [`Self::user`] for cache purposes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,

    /// Options for prompt caching. Supported on `gpt-5.6` and later.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_options: Option<PromptCacheOptions>,

    /// How long the prompt cache should keep its entries.
    /// Deprecated in favor of [`PromptCacheOptions::ttl`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_retention: Option<PromptCacheRetention>,

    /// How hard the model thinks. Not every model supports every tier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,

    /// The shape of the model's answer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ResponseFormat>,

    /// The output modalities the model may produce. `["text"]` by default;
    /// `["audio"]` requires [`Self::audio`] to be set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modalities: Option<Vec<Modality>>,

    /// Configuration for audio output. Required when `modalities` includes
    /// `"audio"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio: Option<AudioConfig>,

    /// Configuration for running moderation on the request input and the
    /// generated output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub moderation: Option<ModerationConfig>,

    /// Static content the model is expected to match, used to short-circuit
    /// the generation when the prefix is already known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prediction: Option<PredictionConfig>,

    /// Constrains how verbose the model's answer is. Only supported on
    /// some newer models.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verbosity: Option<Verbosity>,

    /// Options for the web search built-in tool. When set, the model may
    /// search the web before answering.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub web_search_options: Option<WebSearchOptions>,

    /// The tools the model may call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
    /// How the model picks from those tools.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    /// Whether the model may call more than one tool per turn. Defaults to
    /// `true` on the backends that support it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,

    /// Deprecated: a list of functions the model may call. Use [`Self::tools`]
    /// and [`Self::tool_choice`] instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub functions: Option<Vec<FunctionDefinition>>,

    /// Deprecated: the function-calling form of [`Self::tool_choice`]. Use
    /// [`Self::tool_choice`] instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_call: Option<FunctionCallOption>,

    /// Which capacity serves the request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<ServiceTier>,

    /// Set of 16 key-value pairs the backend can attach to the response.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,

    /// Whether to answer as a stream of chunks. Set by [`Self::streaming`].
    pub stream: bool,
    /// Options for the streaming response, when `stream` is true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,

    /// Extra top-level fields to ship on the request body. For an
    /// OpenAI-compatible backend that extends the spec with its own — the
    /// DeepSeek-style `{"thinking":{"type":"enabled"}}`, a tier of
    /// `reasoning_effort` outside the [`ReasoningEffort`] enum, or any other
    /// vendor field — this is the seam: the caller hands in the JSON value
    /// shaped like the field it wants, and the SDK ships it at the top
    /// level as if it were its own.
    ///
    /// `BTreeMap` for sorted, deterministic output; the field is flattened
    /// onto the request so its entries share the request's top level. The
    /// **contract**:
    ///
    /// - **Serialize**: a key here must not also be a typed field on this
    ///   struct — serde panics on duplicates. The caller who needs to
    ///   override a typed field sends it through here and leaves the typed
    ///   slot `None`.
    /// - **Deserialize**: typed fields take priority. A key that has a typed
    ///   slot (e.g. `reasoning_effort`) is matched against that slot's
    ///   type, and a value the type does not accept is a deserialize
    ///   error — it does not fall through to this map. Fields the struct
    ///   does not name (e.g. `thinking`) do land here.
    ///
    /// In other words: this map is the serialize-side passthrough for any
    /// field the SDK does not model, and the deserialize-side passthrough
    /// for fields the SDK does not model *and* has no typed slot for.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra_body: BTreeMap<String, Value>,
}

impl ChatCompletionRequest {
    /// A non-streaming request for the given model and messages.
    pub fn new(model: impl Into<String>, messages: Vec<ChatCompletionMessageParam>) -> Self {
        Self {
            model: model.into(),
            messages,
            max_tokens: None,
            max_completion_tokens: None,
            temperature: None,
            top_p: None,
            n: None,
            stop: None,
            presence_penalty: None,
            frequency_penalty: None,
            logit_bias: None,
            logprobs: None,
            top_logprobs: None,
            seed: None,
            store: None,
            user: None,
            safety_identifier: None,
            prompt_cache_key: None,
            prompt_cache_options: None,
            prompt_cache_retention: None,
            reasoning_effort: None,
            response_format: None,
            modalities: None,
            audio: None,
            moderation: None,
            prediction: None,
            verbosity: None,
            web_search_options: None,
            tools: None,
            tool_choice: None,
            parallel_tool_calls: None,
            functions: None,
            function_call: None,
            service_tier: None,
            metadata: None,
            stream: false,
            stream_options: None,
            extra_body: BTreeMap::new(),
        }
    }

    /// A streaming request for the given model and messages.
    pub fn streaming(model: impl Into<String>, messages: Vec<ChatCompletionMessageParam>) -> Self {
        let mut request = Self::new(model, messages);
        request.stream = true;
        request
    }

    /// A non-streaming variant of this request.
    pub fn non_streaming(mut self) -> Self {
        self.stream = false;
        self
    }

    /// A streaming variant of this request.
    pub fn into_streaming(mut self) -> Self {
        self.stream = true;
        self
    }

    /// Set `max_tokens`.
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    /// Set `max_completion_tokens`.
    pub fn with_max_completion_tokens(mut self, max_tokens: u32) -> Self {
        self.max_completion_tokens = Some(max_tokens);
        self
    }

    /// Set `temperature`.
    pub fn with_temperature(mut self, temperature: f64) -> Self {
        self.temperature = Some(temperature);
        self
    }

    /// Set `top_p`.
    pub fn with_top_p(mut self, top_p: f64) -> Self {
        self.top_p = Some(top_p);
        self
    }

    /// Set `n`.
    pub fn with_n(mut self, n: u32) -> Self {
        self.n = Some(n);
        self
    }

    /// Set `stop` to a single sequence.
    pub fn with_stop(mut self, stop: impl Into<String>) -> Self {
        self.stop = Some(Stop::Single(stop.into()));
        self
    }

    /// Set `stop` to a list of sequences.
    pub fn with_stop_many(mut self, stop: Vec<String>) -> Self {
        self.stop = Some(Stop::Many(stop));
        self
    }

    /// Set `presence_penalty`.
    pub fn with_presence_penalty(mut self, penalty: f64) -> Self {
        self.presence_penalty = Some(penalty);
        self
    }

    /// Set `frequency_penalty`.
    pub fn with_frequency_penalty(mut self, penalty: f64) -> Self {
        self.frequency_penalty = Some(penalty);
        self
    }

    /// Set `logit_bias`.
    pub fn with_logit_bias(mut self, bias: HashMap<String, i32>) -> Self {
        self.logit_bias = Some(bias);
        self
    }

    /// Set `logprobs`.
    pub fn with_logprobs(mut self, logprobs: bool) -> Self {
        self.logprobs = Some(logprobs);
        self
    }

    /// Set `top_logprobs`. Requires `logprobs: true` on the wire.
    pub fn with_top_logprobs(mut self, top_logprobs: u32) -> Self {
        self.top_logprobs = Some(top_logprobs);
        self
    }

    /// Set `seed`.
    pub fn with_seed(mut self, seed: i64) -> Self {
        self.seed = Some(seed);
        self
    }

    /// Set `store`.
    pub fn with_store(mut self, store: bool) -> Self {
        self.store = Some(store);
        self
    }

    /// Set `user`.
    pub fn with_user(mut self, user: impl Into<String>) -> Self {
        self.user = Some(user.into());
        self
    }

    /// Set `safety_identifier`.
    pub fn with_safety_identifier(mut self, id: impl Into<String>) -> Self {
        self.safety_identifier = Some(id.into());
        self
    }

    /// Set `prompt_cache_key`.
    pub fn with_prompt_cache_key(mut self, key: impl Into<String>) -> Self {
        self.prompt_cache_key = Some(key.into());
        self
    }

    /// Set `prompt_cache_options`.
    pub fn with_prompt_cache_options(mut self, options: PromptCacheOptions) -> Self {
        self.prompt_cache_options = Some(options);
        self
    }

    /// Set `prompt_cache_retention`.
    pub fn with_prompt_cache_retention(mut self, retention: PromptCacheRetention) -> Self {
        self.prompt_cache_retention = Some(retention);
        self
    }

    /// Set `reasoning_effort`.
    pub fn with_reasoning_effort(mut self, effort: ReasoningEffort) -> Self {
        self.reasoning_effort = Some(effort);
        self
    }

    /// Set `response_format`.
    pub fn with_response_format(mut self, format: ResponseFormat) -> Self {
        self.response_format = Some(format);
        self
    }

    /// Set `modalities`.
    pub fn with_modalities(mut self, modalities: Vec<Modality>) -> Self {
        self.modalities = Some(modalities);
        self
    }

    /// Set `audio`.
    pub fn with_audio(mut self, audio: AudioConfig) -> Self {
        self.audio = Some(audio);
        self
    }

    /// Set `moderation`.
    pub fn with_moderation(mut self, moderation: ModerationConfig) -> Self {
        self.moderation = Some(moderation);
        self
    }

    /// Set `prediction`.
    pub fn with_prediction(mut self, prediction: PredictionConfig) -> Self {
        self.prediction = Some(prediction);
        self
    }

    /// Set `verbosity`.
    pub fn with_verbosity(mut self, verbosity: Verbosity) -> Self {
        self.verbosity = Some(verbosity);
        self
    }

    /// Set `web_search_options`.
    pub fn with_web_search_options(mut self, options: WebSearchOptions) -> Self {
        self.web_search_options = Some(options);
        self
    }

    /// Add a tool.
    pub fn with_tool(mut self, tool: Tool) -> Self {
        self.tools.get_or_insert_with(Vec::new).push(tool);
        self
    }

    /// Set the tools, replacing any already set.
    pub fn with_tools(mut self, tools: Vec<Tool>) -> Self {
        self.tools = Some(tools);
        self
    }

    /// Set `tool_choice`.
    pub fn with_tool_choice(mut self, choice: ToolChoice) -> Self {
        self.tool_choice = Some(choice);
        self
    }

    /// Set `parallel_tool_calls`.
    pub fn with_parallel_tool_calls(mut self, parallel: bool) -> Self {
        self.parallel_tool_calls = Some(parallel);
        self
    }

    /// Add a function to the deprecated `functions` list.
    pub fn with_function(mut self, function: FunctionDefinition) -> Self {
        self.functions.get_or_insert_with(Vec::new).push(function);
        self
    }

    /// Set the deprecated `function_call` tool-choice value.
    pub fn with_function_call(mut self, choice: FunctionCallOption) -> Self {
        self.function_call = Some(choice);
        self
    }

    /// Set `service_tier`.
    pub fn with_service_tier(mut self, tier: ServiceTier) -> Self {
        self.service_tier = Some(tier);
        self
    }

    /// Set `metadata`.
    pub fn with_metadata(mut self, metadata: Metadata) -> Self {
        self.metadata = Some(metadata);
        self
    }

    /// Set `stream_options` to ask for the final usage chunk. Implies streaming.
    pub fn with_stream_usage(mut self) -> Self {
        self.stream = true;
        self.stream_options = Some(StreamOptions::with_usage());
        self
    }

    /// Add one entry to [`Self::extra_body`]. See that field for the
    /// contract: the key must not also be a typed field on this struct.
    pub fn with_extra_body(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.extra_body.insert(key.into(), value.into());
        self
    }
}

// ============================================================================
// Response: the full ChatCompletion
// ============================================================================

/// A chat completion the model returned in one piece.
///
/// This is what [`crate::Client::completion`] yields: the whole answer in
/// one struct, with the message and the reason the model stopped.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ChatCompletion {
    /// The endpoint's id for this completion.
    pub id: String,
    /// The choices the model generated, in order. `n` controls how many.
    pub choices: Vec<Choice>,
    /// The Unix timestamp (in seconds) of when the completion was created.
    pub created: i64,
    /// The model that produced the completion.
    pub model: String,
    /// The object type, always `"chat.completion"`.
    pub object: String,
    /// The backend configuration fingerprint. Combine with `seed` to spot
    /// when a backend change has affected determinism.
    #[serde(default)]
    pub system_fingerprint: Option<String>,
    /// Token usage statistics. Present unless `stream_options.include_usage`
    /// was unset on a non-streaming request.
    #[serde(default)]
    pub usage: Option<CompletionUsage>,
    /// The service tier the request actually ran on. May differ from what the
    /// request asked for when the asked tier was unavailable.
    #[serde(default)]
    pub service_tier: Option<ServiceTierOrUnknown>,
    /// The metadata the endpoint attached to the request, when asked.
    #[serde(default)]
    pub metadata: Option<Metadata>,
    /// Moderation results for the request input and the generated output,
    /// when the request asked for moderation.
    #[serde(default)]
    pub moderation: Option<ModerationResult>,
}

/// Moderation results for a Chat Completions request that asked for moderation.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ModerationResult {
    /// Moderation for the request input.
    pub input: ModerationSide,
    /// Moderation for the generated output.
    pub output: ModerationSide,
}

/// One side of a moderation result: either a score or an error.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum ModerationSide {
    /// The moderation result for this side, scored.
    Scored(ModerationScored),
    /// The moderation step errored on this side.
    Error {
        /// The error code.
        code: String,
        /// The error message.
        message: String,
    },
}

/// A scored moderation result: the categories that fired and how strongly.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ModerationScored {
    /// Whether each category fired.
    pub categories: HashMap<String, bool>,
    /// The score for each category.
    pub category_scores: HashMap<String, f64>,
    /// Whether any category fired.
    pub flagged: bool,
    /// The moderation model that produced this result.
    pub model: String,
}

/// One choice in a [`ChatCompletion`].
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Choice {
    /// The reason the model stopped generating tokens.
    pub finish_reason: FinishReason,
    /// The index of this choice in `choices`.
    pub index: u32,
    /// The message the model produced.
    pub message: ChatCompletionMessage,
    /// Per-token log probabilities for the model's output, when the request
    /// asked for logprobs.
    #[serde(default)]
    pub logprobs: Option<ChoiceLogprobs>,
}

/// Per-token log probabilities for a choice. Present when the request asked
/// for `logprobs: true`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ChoiceLogprobs {
    /// Log probabilities for each content token.
    #[serde(default)]
    pub content: Option<Vec<ChatCompletionTokenLogprob>>,
    /// Log probabilities for each refusal token, when the model refused.
    #[serde(default)]
    pub refusal: Option<Vec<ChatCompletionTokenLogprob>>,
}

/// A single token's log probability, plus the top alternatives at that
/// position.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ChatCompletionTokenLogprob {
    /// The token string.
    pub token: String,
    /// The token's UTF-8 bytes, when the token is a multi-byte character.
    #[serde(default)]
    pub bytes: Option<Vec<u8>>,
    /// The token's log probability.
    pub logprob: f64,
    /// The most likely alternatives at this position.
    #[serde(default)]
    pub top_logprobs: Vec<TopLogprob>,
}

/// One of the top logprob alternatives at a token position.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct TopLogprob {
    /// The alternative token.
    pub token: String,
    /// The token's UTF-8 bytes.
    #[serde(default)]
    pub bytes: Option<Vec<u8>>,
    /// The alternative's log probability.
    pub logprob: f64,
}

/// Why the model stopped.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(from = "String")]
pub enum FinishReason {
    /// The model hit a natural stop point or one of the stop sequences.
    Stop,
    /// The ceiling on tokens was reached.
    Length,
    /// The model called one of the tools.
    ToolCalls,
    /// The content was omitted by a content filter.
    ContentFilter,
    /// The model used the deprecated `function_call` field.
    FunctionCall,
    /// The endpoint named a reason this crate does not know yet.
    Unknown(String),
}

impl From<String> for FinishReason {
    fn from(value: String) -> Self {
        match value.as_str() {
            "stop" => FinishReason::Stop,
            "length" => FinishReason::Length,
            "tool_calls" => FinishReason::ToolCalls,
            "content_filter" => FinishReason::ContentFilter,
            "function_call" => FinishReason::FunctionCall,
            _ => FinishReason::Unknown(value),
        }
    }
}

impl FinishReason {
    /// The wire string, as the endpoint wrote it.
    pub fn as_str(&self) -> &str {
        match self {
            FinishReason::Stop => "stop",
            FinishReason::Length => "length",
            FinishReason::ToolCalls => "tool_calls",
            FinishReason::ContentFilter => "content_filter",
            FinishReason::FunctionCall => "function_call",
            FinishReason::Unknown(s) => s.as_str(),
        }
    }
}

/// A chat completion message: the model's answer.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ChatCompletionMessage {
    /// The role of the author. Always `"assistant"` on the response wire.
    pub role: Role,
    /// The text of the message. None when the answer was only tool calls.
    #[serde(default)]
    pub content: Option<String>,
    /// The refusal message, when the model refused.
    #[serde(default)]
    pub refusal: Option<String>,
    /// The tool calls the model made, when it made any.
    #[serde(default)]
    pub tool_calls: Option<Vec<MessageToolCall>>,
    /// Deprecated: the function the model called under the old `function_call`
    /// form. Use [`Self::tool_calls`] instead.
    #[serde(default)]
    pub function_call: Option<FunctionCall>,
    /// The audio the model produced, when the request asked for audio output.
    #[serde(default)]
    pub audio: Option<ChatCompletionAudio>,
    /// Per-segment annotations on the model's text — URL citations from web
    /// search, for instance. Carried on the message, not on each text part.
    #[serde(default)]
    pub annotations: Option<Vec<Annotation>>,
}

/// Per-segment annotations on the model's text.
///
/// Tolerant: an annotation kind this crate does not know yet is preserved
/// as its raw JSON value on [`Annotation::Unknown`], so a response that
/// carries a newer annotation kind still reads.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum Annotation {
    /// A URL citation from a web search, parsed into a structured form.
    UrlCitation {
        /// The kind of annotation, always `"url_citation"`.
        #[serde(rename = "type")]
        r#type: String,
        /// The citation's payload.
        url_citation: UrlCitation,
    },
    /// An annotation kind this crate does not know yet, kept verbatim.
    Unknown(Value),
}

/// A URL citation from a web search annotation.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct UrlCitation {
    /// The index of the last character the citation covers.
    pub end_index: u32,
    /// The index of the first character the citation covers.
    pub start_index: u32,
    /// The title of the cited page.
    pub title: String,
    /// The URL of the cited page.
    pub url: String,
}

/// Audio output the model produced. Present when the request asked for
/// audio output and the model produced some.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ChatCompletionAudio {
    /// The unique id of this audio response.
    pub id: String,
    /// Base64-encoded audio bytes, in the format specified in the request.
    pub data: String,
    /// Unix timestamp after which the audio is no longer accessible.
    pub expires_at: i64,
    /// The transcript of the audio the model generated.
    pub transcript: String,
}

/// A tool call the model made. Tagged by `type`, with the function form the
/// only one this crate models in detail.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum MessageToolCall {
    /// A function tool call.
    #[serde(rename = "function")]
    Function {
        /// The endpoint's id for this call. Replayed in the matching tool result.
        id: String,
        /// The function the model called.
        function: FunctionCall,
    },
}

/// A function tool call the model made: id, name, and a JSON string of arguments.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct FunctionToolCall {
    /// The id of the tool call, as the endpoint named it.
    pub id: String,
    /// The function that was called.
    pub function: FunctionCall,
}

// ============================================================================
// Response: streaming chunks
// ============================================================================

/// One chunk of a streamed completion. The same shape arrives for every chunk;
/// the choices list is empty on the final usage chunk when
/// `stream_options.include_usage` is true.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ChatCompletionChunk {
    /// The endpoint's id for this completion. The same value on every chunk.
    pub id: String,
    /// The choices in this chunk. Usually one, with a delta.
    pub choices: Vec<ChunkChoice>,
    /// The Unix timestamp (in seconds) of when the completion was created.
    pub created: i64,
    /// The model that produced the completion.
    pub model: String,
    /// The object type, always `"chat.completion.chunk"`.
    pub object: String,
    /// The backend configuration fingerprint, when the endpoint reports one.
    #[serde(default)]
    pub system_fingerprint: Option<String>,
    /// Token usage for the whole request. Only present on chunks when
    /// `stream_options.include_usage` was true, and only non-null on the last.
    #[serde(default)]
    pub usage: Option<CompletionUsage>,
    /// The service tier the request actually ran on.
    #[serde(default)]
    pub service_tier: Option<ServiceTierOrUnknown>,
    /// An obfuscation string added by the endpoint to normalize payload
    /// sizes as a side-channel mitigation. Skipped when the request set
    /// `stream_options.include_obfuscation: false`.
    #[serde(default)]
    pub obfuscation: Option<String>,
    /// Moderation results for the request input and the generated output,
    /// when the request asked for moderation.
    #[serde(default)]
    pub moderation: Option<ModerationResult>,
}

/// One choice in a [`ChatCompletionChunk`].
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ChunkChoice {
    /// The delta for this choice: what arrived in this chunk.
    pub delta: ChoiceDelta,
    /// The reason the model stopped, when this chunk carried it. None on
    /// intermediate chunks.
    #[serde(default)]
    pub finish_reason: Option<FinishReason>,
    /// The index of this choice in `choices`.
    pub index: u32,
    /// Per-token log probabilities for this chunk, when the request asked
    /// for them.
    #[serde(default)]
    pub logprobs: Option<ChoiceLogprobs>,
}

/// A delta on a streamed chunk: what arrived in this chunk.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ChoiceDelta {
    /// The role, when this is the first chunk for a new choice.
    #[serde(default)]
    pub role: Option<Role>,
    /// A piece of the assistant's text.
    #[serde(default)]
    pub content: Option<String>,
    /// A piece of the assistant's refusal message, when refusing.
    #[serde(default)]
    pub refusal: Option<String>,
    /// Pieces of the assistant's tool calls. Arrives sharded: the first chunk
    /// carries `id` and `function.name`, the rest carry `function.arguments`.
    #[serde(default)]
    pub tool_calls: Option<Vec<DeltaToolCall>>,
    /// Deprecated: a shard of the function the model called under the old
    /// `function_call` form.
    #[serde(default)]
    pub function_call: Option<DeltaFunctionCall>,
    /// Vendor fields the spec does not describe. DeepSeek and GLM stream the
    /// model's reasoning under `reasoning_content`; the SDK does not name it
    /// (it is not in the spec) and the caller who needs it reads it from
    /// here.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// A tool call delta on a streamed chunk.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct DeltaToolCall {
    /// The index of this tool call in the choice's `tool_calls` list. Stable
    /// across the whole stream, so a caller shards on it.
    pub index: u32,
    /// The id of the tool call, on the chunk that carries it.
    #[serde(default)]
    pub id: Option<String>,
    /// The kind of tool call. Always `"function"` on the wire today.
    #[serde(rename = "type", default)]
    pub r#type: Option<String>,
    /// The function the model called, in pieces.
    #[serde(default)]
    pub function: Option<DeltaFunctionCall>,
}

/// A function call delta on a streamed chunk: a name on the first chunk and
/// pieces of the arguments string after that.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct DeltaFunctionCall {
    /// The function's name, on the first chunk for this call.
    #[serde(default)]
    pub name: Option<String>,
    /// A piece of the function's JSON arguments string. Arrives a shard at a
    /// time; the caller concatenates them.
    #[serde(default)]
    pub arguments: Option<String>,
}

// ============================================================================
// Response: usage
// ============================================================================

/// A breakdown of prompt tokens, when the endpoint reports one.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct PromptTokensDetails {
    /// Tokens served from cache. The number the model didn't have to recompute.
    #[serde(default)]
    pub cached_tokens: Option<u64>,
    /// Tokens that were audio input.
    #[serde(default)]
    pub audio_tokens: Option<u64>,
    /// Tokens that were image input.
    #[serde(default)]
    pub image_tokens: Option<u64>,
    /// Tokens that were text input.
    #[serde(default)]
    pub text_tokens: Option<u64>,
}

/// A breakdown of completion tokens, when the endpoint reports one.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct CompletionTokensDetails {
    /// Reasoning tokens spent before the answer.
    #[serde(default)]
    pub reasoning_tokens: Option<u64>,
    /// Tokens that were audio output.
    #[serde(default)]
    pub audio_tokens: Option<u64>,
    /// Tokens that were text output.
    #[serde(default)]
    pub text_tokens: Option<u64>,
    /// Tokens from a `prediction` that appeared in the answer.
    #[serde(default)]
    pub accepted_prediction_tokens: Option<u64>,
    /// Tokens from a `prediction` that did not appear in the answer.
    #[serde(default)]
    pub rejected_prediction_tokens: Option<u64>,
}

/// Token usage for the request, when the endpoint reports it.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct CompletionUsage {
    /// Tokens the model spent on the prompt.
    pub prompt_tokens: u64,
    /// Tokens the model spent on the answer.
    pub completion_tokens: u64,
    /// `prompt_tokens + completion_tokens`. Not always `==`, when the model
    /// reports it differently than its own arithmetic.
    pub total_tokens: u64,
    /// A breakdown of prompt tokens, when the endpoint reports one.
    #[serde(default)]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
    /// A breakdown of completion tokens, when the endpoint reports one.
    #[serde(default)]
    pub completion_tokens_details: Option<CompletionTokensDetails>,
    /// Vendor fields the spec does not describe. DeepSeek reports
    /// `prompt_cache_hit_tokens` and `prompt_cache_miss_tokens` flat; the
    /// SDK does not name them (they are not in the spec) and the caller
    /// who needs them reads them from here.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

// ============================================================================
// Response: error
// ============================================================================

/// The error body the endpoint returns on a non-2xx response. The shape the
/// reference client deserializes from `{"error": {...}}`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ErrorBody {
    /// The error itself.
    pub error: ErrorObject,
}

/// The error object inside [`ErrorBody`].
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ErrorObject {
    /// The message, as the endpoint wrote it.
    pub message: String,
    /// The error type, when the endpoint named one.
    #[serde(default)]
    pub r#type: Option<String>,
    /// The error code, when the endpoint named one.
    #[serde(default)]
    pub code: Option<String>,
    /// The parameter the endpoint blamed, when it blamed one.
    #[serde(default)]
    pub param: Option<String>,
}

// ============================================================================
// Service tier: tolerant on the response side
// ============================================================================

/// The service tier as the endpoint reports it on the response. Tolerant by
/// construction: a tier this crate does not know yet is read past as
/// [`ServiceTierOrUnknown::Unknown`], so an endpoint that adds one does not
/// fail a response that carries it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum ServiceTierOrUnknown {
    /// A known tier.
    Known(ServiceTier),
    /// A tier this crate does not know yet. The wire string is kept verbatim.
    Unknown(String),
}

impl ServiceTierOrUnknown {
    /// The wire string the endpoint sent, whether known or not.
    pub fn as_str(&self) -> &str {
        match self {
            ServiceTierOrUnknown::Known(tier) => match tier {
                ServiceTier::Auto => "auto",
                ServiceTier::Default => "default",
                ServiceTier::Flex => "flex",
                ServiceTier::Scale => "scale",
                ServiceTier::Priority => "priority",
            },
            ServiceTierOrUnknown::Unknown(s) => s.as_str(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_minimal_request_serializes_to_what_the_endpoint_reads() {
        let request =
            ChatCompletionRequest::new("gpt-4o", vec![ChatCompletionMessageParam::user("hi")]);
        let value = serde_json::to_value(&request).unwrap();
        // Only the two required fields go out.
        assert_eq!(
            value,
            json!({
                "model": "gpt-4o",
                "messages": [{"role": "user", "content": "hi"}],
                "stream": false,
            })
        );
    }

    #[test]
    fn a_streaming_request_with_a_tool_and_options_round_trips() {
        let request = ChatCompletionRequest::streaming(
            "gpt-4o",
            vec![ChatCompletionMessageParam::user("hi")],
        )
        .with_max_tokens(1024)
        .with_temperature(0.7)
        .with_reasoning_effort(ReasoningEffort::Medium)
        .with_tool(Tool::function(FunctionDefinition::new(
            "get_weather",
            json!({"type": "object"}),
        )))
        .with_tool_choice(ToolChoice::auto())
        .with_stream_usage();
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["stream"], json!(true));
        assert_eq!(value["stream_options"]["include_usage"], json!(true));
        assert_eq!(value["max_tokens"], json!(1024));
        assert_eq!(value["temperature"], json!(0.7));
        assert_eq!(value["reasoning_effort"], json!("medium"));
        assert_eq!(value["tools"][0]["type"], json!("function"));
        assert_eq!(value["tools"][0]["function"]["name"], json!("get_weather"));
        assert_eq!(value["tool_choice"], json!("auto"));

        // Back to itself: the wire form is the same shape the endpoint reads.
        let back: ChatCompletionRequest = serde_json::from_value(value).unwrap();
        assert_eq!(back, request);
    }

    /// Vendor fields the spec does not describe — DeepSeek's
    /// `thinking` and an `extra_body` map keyed by field name — land at
    /// the top level of the request body. The contract: a key here must
    /// not also be a typed field, or serde panics on the duplicate.
    #[test]
    fn extra_body_flattens_to_the_request_top_level() {
        let request = ChatCompletionRequest::streaming(
            "deepseek-v4",
            vec![ChatCompletionMessageParam::user("hi")],
        )
        .with_extra_body("thinking", json!({"type": "enabled"}));
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["thinking"], json!({"type": "enabled"}));
        // `thinking` is not a typed field, so it round-trips through extra.
        let back: ChatCompletionRequest = serde_json::from_value(value).unwrap();
        assert_eq!(back, request);
    }

    /// An empty `extra_body` is not written: a request that has nothing
    /// vendor-specific carries no `extra_body` key on the wire.
    #[test]
    fn extra_body_skips_when_empty() {
        let request = ChatCompletionRequest::new("gpt-4o", vec![]);
        let value = serde_json::to_value(&request).unwrap();
        assert!(value.get("extra_body").is_none(), "{value}");
        assert!(value.as_object().unwrap().keys().all(|k| k != "extra_body"));
    }

    #[test]
    fn user_message_with_images_carries_them_as_parts() {
        let msg = UserMessageParam::with_images(
            "what is this?",
            vec!["data:image/png;base64,AAAA".into()],
        );
        // The role is on the enum wrapper, not on the inner struct.
        let value = serde_json::to_value(ChatCompletionMessageParam::User(msg)).unwrap();
        assert_eq!(value["role"], json!("user"));
        let parts = value["content"].as_array().unwrap();
        assert_eq!(parts[0]["type"], json!("text"));
        assert_eq!(parts[0]["text"], json!("what is this?"));
        assert_eq!(parts[1]["type"], json!("image_url"));
        assert_eq!(
            parts[1]["image_url"]["url"],
            json!("data:image/png;base64,AAAA")
        );
    }

    #[test]
    fn user_message_with_no_images_stays_a_string() {
        // The point: a request that doesn't need the parts form doesn't carry
        // it, because history replays byte-for-byte.
        let msg = ChatCompletionMessageParam::user("just text");
        let value = serde_json::to_value(&msg).unwrap();
        assert_eq!(value, json!({"role": "user", "content": "just text"}));
    }

    #[test]
    fn a_finish_reason_is_decoded_leniently() {
        let choice: Choice = serde_json::from_value(json!({
            "finish_reason": "future_reason",
            "index": 0,
            "message": {"role": "assistant", "content": null}
        }))
        .unwrap();
        assert_eq!(choice.finish_reason.as_str(), "future_reason");
        assert!(matches!(choice.finish_reason, FinishReason::Unknown(_)));
    }

    #[test]
    fn a_known_finish_reason_is_decoded_as_its_variant() {
        let choice: Choice = serde_json::from_value(json!({
            "finish_reason": "tool_calls",
            "index": 0,
            "message": {"role": "assistant", "content": null}
        }))
        .unwrap();
        assert_eq!(choice.finish_reason, FinishReason::ToolCalls);
    }

    #[test]
    fn a_response_carries_unknown_fields_without_failing() {
        // The endpoint adds fields over time. A response that has them should
        // still parse; the new fields are dropped, not refused.
        let value = json!({
            "id": "cmpl-1",
            "choices": [{
                "finish_reason": "stop",
                "index": 0,
                "message": {"role": "assistant", "content": "hi"},
                "logprobs": null,
            }],
            "created": 1,
            "model": "gpt-4o",
            "object": "chat.completion",
            "future_field": {"something": true},
        });
        let parsed: ChatCompletion = serde_json::from_value(value).unwrap();
        assert_eq!(parsed.id, "cmpl-1");
        assert_eq!(parsed.choices[0].message.content.as_deref(), Some("hi"));
    }

    #[test]
    fn service_tier_on_response_tolerates_unknown_values() {
        let parsed: ServiceTierOrUnknown = serde_json::from_value(json!("new_tier")).unwrap();
        assert!(matches!(parsed, ServiceTierOrUnknown::Unknown(ref s) if s == "new_tier"));
        let parsed: ServiceTierOrUnknown = serde_json::from_value(json!("auto")).unwrap();
        assert_eq!(parsed, ServiceTierOrUnknown::Known(ServiceTier::Auto));
    }

    #[test]
    fn a_stream_chunk_parses_the_way_the_endpoint_writes_it() {
        let value = json!({
            "id": "cmpl-1",
            "choices": [{
                "delta": {"role": "assistant", "content": "hi"},
                "finish_reason": null,
                "index": 0,
            }],
            "created": 1,
            "model": "gpt-4o",
            "object": "chat.completion.chunk",
        });
        let chunk: ChatCompletionChunk = serde_json::from_value(value).unwrap();
        assert_eq!(chunk.choices[0].delta.content.as_deref(), Some("hi"));
        assert_eq!(chunk.choices[0].index, 0);
    }

    #[test]
    fn a_tool_call_delta_arrives_sharded_with_index_and_id() {
        let value = json!({
            "id": "cmpl-1",
            "choices": [{
                "delta": {"tool_calls": [{
                    "index": 0,
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "get_weather", "arguments": ""}
                }]},
                "finish_reason": null,
                "index": 0,
            }],
            "created": 1,
            "model": "gpt-4o",
            "object": "chat.completion.chunk",
        });
        let chunk: ChatCompletionChunk = serde_json::from_value(value).unwrap();
        let tc = &chunk.choices[0].delta.tool_calls.as_ref().unwrap()[0];
        assert_eq!(tc.index, 0);
        assert_eq!(tc.id.as_deref(), Some("call_1"));
        assert_eq!(
            tc.function.as_ref().unwrap().name.as_deref(),
            Some("get_weather")
        );
    }

    #[test]
    fn usage_parses_with_or_without_breakdowns() {
        let usage: CompletionUsage = serde_json::from_value(json!({
            "prompt_tokens": 10,
            "completion_tokens": 5,
            "total_tokens": 15,
        }))
        .unwrap();
        assert_eq!(usage.prompt_tokens, 10);
        assert!(usage.prompt_tokens_details.is_none());

        let usage: CompletionUsage = serde_json::from_value(json!({
            "prompt_tokens": 10,
            "completion_tokens": 5,
            "total_tokens": 15,
            "prompt_tokens_details": {"cached_tokens": 8}
        }))
        .unwrap();
        assert_eq!(usage.prompt_tokens_details.unwrap().cached_tokens, Some(8));
    }

    /// DeepSeek reports `prompt_cache_hit_tokens` and
    /// `prompt_cache_miss_tokens` flat on the usage block — not in any
    /// typed slot. The passthrough catches them so a caller reading cache
    /// hit/miss has them in `extra`.
    #[test]
    fn usage_extra_captures_deepseek_flat_cache_fields() {
        let usage: CompletionUsage = serde_json::from_value(json!({
            "prompt_tokens": 17,
            "completion_tokens": 9,
            "total_tokens": 26,
            "prompt_cache_hit_tokens": 8,
            "prompt_cache_miss_tokens": 9,
        }))
        .unwrap();
        assert_eq!(usage.extra.get("prompt_cache_hit_tokens"), Some(&json!(8)));
        assert_eq!(usage.extra.get("prompt_cache_miss_tokens"), Some(&json!(9)));
    }

    /// DeepSeek/GLM stream reasoning under `reasoning_content` on the
    /// delta; the SDK does not name the field (it is not on the spec) and
    /// the caller reads it from `extra`.
    #[test]
    fn delta_extra_captures_reasoning_content() {
        let delta: ChoiceDelta = serde_json::from_value(json!({
            "reasoning_content": "let me think…"
        }))
        .unwrap();
        assert_eq!(
            delta.extra.get("reasoning_content"),
            Some(&json!("let me think…"))
        );
        assert!(delta.content.is_none());
    }

    #[test]
    fn response_format_text_json_object_and_schema_each_serialize() {
        let value = serde_json::to_value(ResponseFormat::text()).unwrap();
        assert_eq!(value, json!({"type": "text"}));

        let value = serde_json::to_value(ResponseFormat::json_object()).unwrap();
        assert_eq!(value, json!({"type": "json_object"}));

        let value = serde_json::to_value(ResponseFormat::json_schema(JsonSchemaSpec::new(
            "answer",
            json!({"type": "object"}),
        )))
        .unwrap();
        assert_eq!(value["type"], json!("json_schema"));
        assert_eq!(value["json_schema"]["name"], json!("answer"));
    }

    #[test]
    fn message_content_text_and_parts_constructors_extract_text() {
        // The text constructor wraps a plain string.
        let content = MessageContent::text("hi");
        assert!(matches!(content, MessageContent::Text(ref s) if s == "hi"));
        assert_eq!(content.as_text(), "hi");

        // The parts constructor wraps an array of parts; `as_text` joins the
        // text parts and skips the image parts.
        let content = MessageContent::parts(vec![
            ContentPart::text("a"),
            ContentPart::image_url("data:image/png;base64,AAAA"),
            ContentPart::text("b"),
        ]);
        assert_eq!(content.as_text(), "a\nb");
    }

    #[test]
    fn every_message_constructor_carries_the_role_it_says() {
        // The point of the role-tagged enum: a round-trip preserves which role
        // each message was, so history replays byte-for-byte.
        let messages = vec![
            ChatCompletionMessageParam::Developer(DeveloperMessageParam::new("d")),
            ChatCompletionMessageParam::System(SystemMessageParam::new("s")),
            ChatCompletionMessageParam::User(UserMessageParam::new("u")),
            ChatCompletionMessageParam::Assistant(AssistantMessageParam::new("a")),
            ChatCompletionMessageParam::Tool(ToolMessageParam::new("call_1", "t")),
            ChatCompletionMessageParam::Function(FunctionMessageParam::new("f", "fn")),
        ];
        let value = serde_json::to_value(&messages).unwrap();
        let arr = value.as_array().unwrap();
        assert_eq!(arr[0]["role"], json!("developer"));
        assert_eq!(arr[1]["role"], json!("system"));
        assert_eq!(arr[2]["role"], json!("user"));
        assert_eq!(arr[3]["role"], json!("assistant"));
        assert_eq!(arr[4]["role"], json!("tool"));
        assert_eq!(arr[5]["role"], json!("function"));
    }

    #[test]
    fn message_param_helpers_match_their_enum_variants() {
        // The convenience constructors on the enum match the variants the
        // module names them after.
        assert!(matches!(
            ChatCompletionMessageParam::user("hi"),
            ChatCompletionMessageParam::User(_)
        ));
        assert!(matches!(
            ChatCompletionMessageParam::system("hi"),
            ChatCompletionMessageParam::System(_)
        ));
        assert!(matches!(
            ChatCompletionMessageParam::developer("hi"),
            ChatCompletionMessageParam::Developer(_)
        ));
        assert!(matches!(
            ChatCompletionMessageParam::assistant("hi"),
            ChatCompletionMessageParam::Assistant(_)
        ));
        assert!(matches!(
            ChatCompletionMessageParam::tool("call_1", "hi"),
            ChatCompletionMessageParam::Tool(_)
        ));
    }

    #[test]
    fn tool_choice_serializes_each_form_the_endpoint_accepts() {
        let value = serde_json::to_value(ToolChoice::auto()).unwrap();
        assert_eq!(value, json!("auto"));
        let value = serde_json::to_value(ToolChoice::none()).unwrap();
        assert_eq!(value, json!("none"));
        let value = serde_json::to_value(ToolChoice::required()).unwrap();
        assert_eq!(value, json!("required"));
        let value = serde_json::to_value(ToolChoice::function("get_weather")).unwrap();
        assert_eq!(value["type"], json!("function"));
        assert_eq!(value["function"]["name"], json!("get_weather"));

        // Allowed: constrains the model to a pre-defined set.
        let value = serde_json::to_value(ToolChoice::allowed(
            vec![
                ChatCompletionAllowedToolRef::Function {
                    function: AllowedFunctionFields {
                        name: "get_weather".into(),
                    },
                },
                ChatCompletionAllowedToolRef::Custom {
                    name: "my_tool".into(),
                },
            ],
            ChatCompletionAllowedToolMode::Required,
        ))
        .unwrap();
        assert_eq!(value["type"], json!("allowed_tools"));
        assert_eq!(value["allowed_tools"]["mode"], json!("required"));
        assert_eq!(
            value["allowed_tools"]["tools"][0]["type"],
            json!("function")
        );
        assert_eq!(
            value["allowed_tools"]["tools"][0]["name"],
            json!("get_weather")
        );
        assert_eq!(value["allowed_tools"]["tools"][1]["type"], json!("custom"));
        assert_eq!(value["allowed_tools"]["tools"][1]["name"], json!("my_tool"));

        // Custom: force the model to call a specific custom tool.
        let value = serde_json::to_value(ToolChoice::custom("my_tool")).unwrap();
        assert_eq!(value["type"], json!("custom"));
        assert_eq!(value["custom"]["name"], json!("my_tool"));
    }

    #[test]
    fn stop_serialization_handles_single_and_many() {
        let value = serde_json::to_value(Stop::Single("END".into())).unwrap();
        assert_eq!(value, json!("END"));
        let value = serde_json::to_value(Stop::Many(vec!["END".into(), "STOP".into()])).unwrap();
        assert_eq!(value, json!(["END", "STOP"]));
    }

    #[test]
    fn function_definition_with_description_and_strict_serializes() {
        let def = FunctionDefinition::new("get_weather", json!({"type": "object"}))
            .with_description("Look up the weather")
            .with_strict(true);
        let value = serde_json::to_value(def).unwrap();
        assert_eq!(value["name"], json!("get_weather"));
        assert_eq!(value["description"], json!("Look up the weather"));
        assert_eq!(value["strict"], json!(true));
    }

    #[test]
    fn json_schema_spec_with_strict_and_description_serializes() {
        let spec = JsonSchemaSpec::new("answer", json!({"type": "object"}))
            .with_strict(true)
            .with_description("the answer");
        let value = serde_json::to_value(spec).unwrap();
        assert_eq!(value["strict"], json!(true));
        assert_eq!(value["description"], json!("the answer"));
    }

    #[test]
    fn content_part_helpers_construct_each_variant() {
        let text = ContentPart::text("hi");
        assert!(matches!(text, ContentPart::Text { ref text, .. } if text == "hi"));
        let image = ContentPart::image_url("data:...");
        assert!(matches!(image, ContentPart::ImageUrl { .. }));
        assert!(image.text_value().is_none());
    }

    #[test]
    fn image_url_detail_field_is_optional() {
        // detail omitted when None — the optional field is skipped.
        let url = ImageUrl {
            url: "data:image/png;base64,AAAA".into(),
            detail: None,
        };
        let value = serde_json::to_value(&url).unwrap();
        assert!(value.get("detail").is_none());
        assert_eq!(value["url"], json!("data:image/png;base64,AAAA"));
    }

    #[test]
    fn assistant_message_with_tool_calls_round_trips() {
        let calls = vec![MessageToolCall::Function {
            id: "call_1".into(),
            function: FunctionCall {
                name: "get_weather".into(),
                arguments: "{\"city\":\"sf\"}".into(),
            },
        }];
        let msg = AssistantMessageParam::with_tool_calls(calls.clone()).with_name("claude");
        let value = serde_json::to_value(ChatCompletionMessageParam::Assistant(msg)).unwrap();
        assert_eq!(value["role"], json!("assistant"));
        assert_eq!(value["name"], json!("claude"));
        assert!(value["content"].is_null());
        assert_eq!(value["tool_calls"][0]["id"], json!("call_1"));
        assert_eq!(value["tool_calls"][0]["type"], json!("function"));
        let back: ChatCompletionMessageParam = serde_json::from_value(value).unwrap();
        let AssistantMessageParam { tool_calls, .. } = match back {
            ChatCompletionMessageParam::Assistant(inner) => inner,
            _ => panic!("expected assistant"),
        };
        assert_eq!(tool_calls, Some(calls));
    }

    #[test]
    fn function_message_with_name_round_trips() {
        let msg = FunctionMessageParam::new("get_weather", "sunny");
        let value = serde_json::to_value(ChatCompletionMessageParam::Function(msg)).unwrap();
        assert_eq!(value["role"], json!("function"));
        assert_eq!(value["name"], json!("get_weather"));
        assert_eq!(value["content"], json!("sunny"));
    }

    #[test]
    fn tool_message_with_tool_call_id_round_trips() {
        let msg = ToolMessageParam::new("call_1", "sunny");
        let value = serde_json::to_value(ChatCompletionMessageParam::Tool(msg)).unwrap();
        assert_eq!(value["role"], json!("tool"));
        assert_eq!(value["tool_call_id"], json!("call_1"));
        assert_eq!(value["content"], json!("sunny"));
    }

    #[test]
    fn every_request_builder_round_trips() {
        // The request builders the spec needs, end to end. Optional fields
        // that are unset do not appear in the JSON.
        let request =
            ChatCompletionRequest::new("gpt-4o", vec![ChatCompletionMessageParam::user("hi")])
                .with_max_completion_tokens(2048)
                .with_temperature(0.7)
                .with_top_p(0.9)
                .with_n(2)
                .with_stop_many(vec!["END".into(), "STOP".into()])
                .with_presence_penalty(0.1)
                .with_frequency_penalty(0.2)
                .with_seed(42)
                .with_user("user-1")
                .with_reasoning_effort(ReasoningEffort::High)
                .with_parallel_tool_calls(false)
                .with_service_tier(ServiceTier::Auto)
                .with_metadata([("trace_id".into(), "abc".into())].into_iter().collect());
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["max_completion_tokens"], json!(2048));
        assert_eq!(value["temperature"], json!(0.7));
        assert_eq!(value["top_p"], json!(0.9));
        assert_eq!(value["n"], json!(2));
        assert_eq!(value["stop"], json!(["END", "STOP"]));
        assert_eq!(value["presence_penalty"], json!(0.1));
        assert_eq!(value["frequency_penalty"], json!(0.2));
        assert_eq!(value["seed"], json!(42));
        assert_eq!(value["user"], json!("user-1"));
        assert_eq!(value["reasoning_effort"], json!("high"));
        assert_eq!(value["parallel_tool_calls"], json!(false));
        assert_eq!(value["service_tier"], json!("auto"));
        assert_eq!(value["metadata"]["trace_id"], json!("abc"));
        let back: ChatCompletionRequest = serde_json::from_value(value).unwrap();
        assert_eq!(back, request);
    }

    #[test]
    fn finish_reason_unknown_variants_keep_their_wire_string() {
        // An unknown reason: the wire string is kept so the value passes
        // through the type system without losing information.
        assert_eq!(FinishReason::Stop.as_str(), "stop");
        assert_eq!(FinishReason::Length.as_str(), "length");
        assert_eq!(FinishReason::ToolCalls.as_str(), "tool_calls");
        assert_eq!(FinishReason::ContentFilter.as_str(), "content_filter");
        assert_eq!(FinishReason::FunctionCall.as_str(), "function_call");
        assert_eq!(
            FinishReason::Unknown("future_reason".into()).as_str(),
            "future_reason"
        );
    }

    #[test]
    fn a_full_chat_completion_response_deserializes() {
        let value = json!({
            "id": "cmpl-1",
            "choices": [{
                "finish_reason": "tool_calls",
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "get_weather", "arguments": "{}"}
                    }]
                }
            }],
            "created": 1,
            "model": "gpt-4o",
            "object": "chat.completion",
            "system_fingerprint": "fp_1",
            "service_tier": "auto",
            "metadata": {"k": "v"},
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15,
                "prompt_tokens_details": {"cached_tokens": 8}
            }
        });
        let completion: ChatCompletion = serde_json::from_value(value).unwrap();
        assert_eq!(completion.id, "cmpl-1");
        assert_eq!(completion.model, "gpt-4o");
        assert_eq!(completion.choices[0].finish_reason, FinishReason::ToolCalls);
        let calls = completion.choices[0].message.tool_calls.as_ref().unwrap();
        match &calls[0] {
            MessageToolCall::Function { id, function } => {
                assert_eq!(id, "call_1");
                assert_eq!(function.name, "get_weather");
            }
        }
        let usage = completion.usage.unwrap();
        assert_eq!(usage.prompt_tokens_details.unwrap().cached_tokens, Some(8));
    }

    #[test]
    fn an_error_body_deserializes() {
        let value = json!({
            "error": {
                "message": "Incorrect API key",
                "type": "invalid_request_error",
                "code": "invalid_api_key",
                "param": null,
            }
        });
        let body: ErrorBody = serde_json::from_value(value).unwrap();
        assert_eq!(body.error.message, "Incorrect API key");
        assert_eq!(body.error.code.as_deref(), Some("invalid_api_key"));
    }

    // ─────── New request fields ───────

    #[test]
    fn logit_bias_and_logprobs_round_trip() {
        let mut bias = std::collections::HashMap::new();
        bias.insert("50256".to_string(), -100);
        bias.insert("1234".to_string(), 50);
        let request =
            ChatCompletionRequest::new("gpt-4o", vec![ChatCompletionMessageParam::user("hi")])
                .with_logit_bias(bias.clone())
                .with_logprobs(true)
                .with_top_logprobs(5);
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["logit_bias"]["50256"], json!(-100));
        assert_eq!(value["logprobs"], json!(true));
        assert_eq!(value["top_logprobs"], json!(5));
        let back: ChatCompletionRequest = serde_json::from_value(value).unwrap();
        assert_eq!(back.logit_bias.unwrap()["50256"], -100);
        assert_eq!(back.top_logprobs, Some(5));
    }

    #[test]
    fn cache_and_safety_fields_round_trip() {
        let request =
            ChatCompletionRequest::new("gpt-4o", vec![ChatCompletionMessageParam::user("hi")])
                .with_store(true)
                .with_safety_identifier("safe-1")
                .with_prompt_cache_key("cache-1")
                .with_prompt_cache_options(
                    PromptCacheOptions::new()
                        .with_mode(PromptCacheMode::Explicit)
                        .with_comparison_response_id("resp-1"),
                )
                .with_prompt_cache_retention(PromptCacheRetention::TwentyFourHours);
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["store"], json!(true));
        assert_eq!(value["safety_identifier"], json!("safe-1"));
        assert_eq!(value["prompt_cache_key"], json!("cache-1"));
        assert_eq!(value["prompt_cache_options"]["mode"], json!("explicit"));
        assert_eq!(value["prompt_cache_retention"], json!("24h"));
        let back: ChatCompletionRequest = serde_json::from_value(value).unwrap();
        assert_eq!(back.store, Some(true));
        assert_eq!(back.safety_identifier.as_deref(), Some("safe-1"));
        assert_eq!(
            back.prompt_cache_options
                .unwrap()
                .comparison_response_id
                .as_deref(),
            Some("resp-1")
        );
    }

    #[test]
    fn audio_modalities_and_voice_serialize() {
        let request =
            ChatCompletionRequest::new("gpt-4o", vec![ChatCompletionMessageParam::user("hi")])
                .with_modalities(vec![Modality::Text, Modality::Audio])
                .with_audio(AudioConfig::new(AudioFormat::Mp3, "alloy"))
                .with_verbosity(Verbosity::Low);
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["modalities"], json!(["text", "audio"]));
        assert_eq!(value["audio"]["format"], json!("mp3"));
        assert_eq!(value["audio"]["voice"], json!("alloy"));
        assert_eq!(value["verbosity"], json!("low"));
    }

    #[test]
    fn prediction_and_moderation_serialize() {
        let request =
            ChatCompletionRequest::new("gpt-4o", vec![ChatCompletionMessageParam::user("hi")])
                .with_prediction(PredictionConfig::text("the quick brown fox"))
                .with_moderation(ModerationConfig::new("omni-moderation-latest").with_policy(
                    ModerationPolicy {
                        input: Some(ModerationPolicyInput {
                            mode: ModerationMode::Score,
                        }),
                        output: Some(ModerationPolicyOutput {
                            mode: ModerationMode::Block,
                        }),
                    },
                ));
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["prediction"]["type"], json!("content"));
        assert_eq!(value["prediction"]["content"], json!("the quick brown fox"));
        assert_eq!(
            value["moderation"]["model"],
            json!("omni-moderation-latest")
        );
        assert_eq!(
            value["moderation"]["policy"]["input"]["mode"],
            json!("score")
        );
        assert_eq!(
            value["moderation"]["policy"]["output"]["mode"],
            json!("block")
        );
    }

    #[test]
    fn deprecated_functions_and_function_call_serialize() {
        let request =
            ChatCompletionRequest::new("gpt-4o", vec![ChatCompletionMessageParam::user("hi")])
                .with_function(FunctionDefinition::new(
                    "get_weather",
                    json!({"type":"object"}),
                ))
                .with_function_call(FunctionCallOption::named("get_weather"));
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["functions"][0]["name"], json!("get_weather"));
        assert_eq!(value["function_call"]["name"], json!("get_weather"));
    }

    #[test]
    fn web_search_options_serialize() {
        let request =
            ChatCompletionRequest::new("gpt-4o", vec![ChatCompletionMessageParam::user("hi")])
                .with_web_search_options(
                    WebSearchOptions::new()
                        .with_search_context_size(SearchContextSize::High)
                        .with_user_location(
                            UserLocation::country("US").with_timezone("America/Los_Angeles"),
                        ),
                );
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(
            value["web_search_options"]["search_context_size"],
            json!("high")
        );
        assert_eq!(
            value["web_search_options"]["user_location"]["country"],
            json!("US")
        );
        assert_eq!(
            value["web_search_options"]["user_location"]["timezone"],
            json!("America/Los_Angeles")
        );
    }

    #[test]
    fn stream_options_with_obfuscation_round_trip() {
        let opts = StreamOptions::without_obfuscation();
        let value = serde_json::to_value(&opts).unwrap();
        assert_eq!(value["include_obfuscation"], json!(false));
    }

    // ─────── New response / chunk fields ───────

    #[test]
    fn choice_logprobs_deserializes() {
        let value = json!({
            "finish_reason": "stop",
            "index": 0,
            "message": {"role": "assistant", "content": "hi"},
            "logprobs": {
                "content": [
                    {"token": "hi", "logprob": -0.1, "top_logprobs": [
                        {"token": "hi", "logprob": -0.1},
                        {"token": "hey", "logprob": -1.2}
                    ]}
                ]
            }
        });
        let choice: Choice = serde_json::from_value(value).unwrap();
        let logprobs = choice.logprobs.unwrap();
        let tokens = logprobs.content.unwrap();
        assert_eq!(tokens[0].token, "hi");
        assert!((tokens[0].logprob - -0.1).abs() < 1e-6);
        assert_eq!(tokens[0].top_logprobs.len(), 2);
    }

    #[test]
    fn message_annotations_and_audio_deserialize() {
        let value = json!({
            "role": "assistant",
            "content": "see [docs](https://example.com)",
            "annotations": [{
                "type": "url_citation",
                "url_citation": {
                    "end_index": 17,
                    "start_index": 5,
                    "title": "Docs",
                    "url": "https://example.com"
                }
            }],
            "audio": {
                "id": "audio_1",
                "data": "AAAA",
                "expires_at": 1700000000,
                "transcript": "hello"
            }
        });
        let message: ChatCompletionMessage = serde_json::from_value(value).unwrap();
        let annotations = message.annotations.unwrap();
        match &annotations[0] {
            Annotation::UrlCitation { url_citation, .. } => {
                assert_eq!(url_citation.url, "https://example.com");
            }
            _ => panic!("expected UrlCitation"),
        }
        let audio = message.audio.unwrap();
        assert_eq!(audio.id, "audio_1");
        assert_eq!(audio.transcript, "hello");
    }

    #[test]
    fn message_unknown_annotation_is_preserved() {
        // An annotation kind the spec adds later: the JSON is kept verbatim
        // on Annotation::Unknown.
        let value = json!({
            "type": "future_annotation",
            "x": 1,
            "y": "z"
        });
        let annotation: Annotation = serde_json::from_value(value).unwrap();
        match annotation {
            Annotation::Unknown(v) => {
                assert_eq!(v["type"], json!("future_annotation"));
                assert_eq!(v["x"], json!(1));
            }
            _ => panic!("expected Unknown"),
        }
    }

    #[test]
    fn chunk_with_obfuscation_deserializes() {
        let value = json!({
            "id": "cmpl-1",
            "choices": [],
            "created": 1,
            "model": "gpt-4o",
            "object": "chat.completion.chunk",
            "obfuscation": "abcdef123456"
        });
        let chunk: ChatCompletionChunk = serde_json::from_value(value).unwrap();
        assert_eq!(chunk.obfuscation.as_deref(), Some("abcdef123456"));
    }
}
