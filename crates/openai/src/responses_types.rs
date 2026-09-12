//! The Responses API as types.
//!
//! Two halves that never meet. The **request** half is serialized and never
//! parsed: its field names are the API's field names, so a glance at a type is
//! a glance at the JSON. The **response** half is parsed and never serialized,
//! and it is tolerant by construction — the spec says event types and enum
//! values may be added, so every enum has a catch-all and unknown object fields
//! are ignored rather than refused.
//!
//! What is here is the published Responses API: the request body field for
//! field, the answer's output items, the streaming events, the usage counts,
//! the input-item variants (messages, tool results, references), the tool
//! definitions and the tool calls the model may emit.
//!
//! What is deliberately not here: the audio/image modalities, the web search
//! and file search tools (only [`ResponseTool::Function`] is modeled in
//! detail), the MCP / shell / computer-use / apply-patch tools, the
//! programmatic tool calling, the prompt-cache options, the context
//! management entries, and the moderation policy. Those are either an
//! advanced feature on top of the Responses API or a different product
//! surface; a call this crate cannot make is a type nobody can check.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ============================================================================
// Request: input items
// ============================================================================

/// The contents of an input message. A plain string, or the array of parts
/// that carries images. Untagged, with the string tried first, so a stored
/// text message is read and written back as the very string it was.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponseInputContent {
    /// A plain string, with no parts form.
    Text(String),
    /// The parts form, when the message carries images or other modalities.
    Parts(Vec<ResponseInputContentPart>),
}

/// One part of an input message's content: a run of text, or an image.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseInputContentPart {
    /// A run of text. The simple form.
    #[serde(rename = "input_text")]
    InputText {
        /// The text itself.
        text: String,
    },
    /// An image, addressed by URL or by a base64 `data:` URL.
    #[serde(rename = "input_image")]
    InputImage {
        /// The image's source URL.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        image_url: Option<String>,
        /// The image file id, when one was uploaded.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file_id: Option<String>,
        /// How much of the image to spend tokens on. The endpoint's default is `auto`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<ResponseImageDetail>,
    },
}

impl From<&str> for ResponseInputContent {
    fn from(text: &str) -> Self {
        ResponseInputContent::Text(text.to_owned())
    }
}

impl From<String> for ResponseInputContent {
    fn from(text: String) -> Self {
        ResponseInputContent::Text(text)
    }
}

impl ResponseInputContentPart {
    /// A text part with the given text.
    pub fn text(text: impl Into<String>) -> Self {
        ResponseInputContentPart::InputText { text: text.into() }
    }

    /// An image part with the given URL.
    pub fn image_url(url: impl Into<String>) -> Self {
        ResponseInputContentPart::InputImage {
            image_url: Some(url.into()),
            file_id: None,
            detail: None,
        }
    }

    /// The text this part carries, when it is a text part. None when it is an
    /// image, because the bytes are not text.
    pub fn text_value(&self) -> Option<&str> {
        match self {
            ResponseInputContentPart::InputText { text } => Some(text.as_str()),
            ResponseInputContentPart::InputImage { .. } => None,
        }
    }
}

/// The detail level for image parts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResponseImageDetail {
    /// The model's own pick.
    Auto,
    /// Spend fewer tokens — enough for a quick glance.
    Low,
    /// Spend more tokens — pixel-level resolution.
    High,
    /// Original resolution, no compression.
    Original,
}

/// The role of an input message on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InputMessageRole {
    /// `system` — top-level instructions.
    System,
    /// `developer` — instructions introduced with the o-series models.
    Developer,
    /// `user` — what the end user said.
    User,
    /// `assistant` — what the model said in a previous turn.
    Assistant,
}

/// An input message to the model. Tagged by `type: "message"`, with the role
/// distinguishing user / system / developer / assistant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EasyInputMessage {
    /// The contents of the message: a plain string or a list of parts.
    pub content: ResponseInputContent,
    /// The role of the message.
    pub role: InputMessageRole,
    /// The type of the item, always `"message"`. The serializer writes it.
    #[serde(rename = "type")]
    pub r#type: ResponseInputItemType,
    /// Whether this is a "commentary" (intermediate thinking) or a "final answer".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<MessagePhase>,
}

impl EasyInputMessage {
    /// A user message with the given text.
    pub fn user(content: impl Into<ResponseInputContent>) -> Self {
        Self {
            content: content.into(),
            role: InputMessageRole::User,
            r#type: ResponseInputItemType::Message,
            phase: None,
        }
    }

    /// A system message with the given text.
    pub fn system(content: impl Into<ResponseInputContent>) -> Self {
        Self {
            content: content.into(),
            role: InputMessageRole::System,
            r#type: ResponseInputItemType::Message,
            phase: None,
        }
    }

    /// A developer message with the given text.
    pub fn developer(content: impl Into<ResponseInputContent>) -> Self {
        Self {
            content: content.into(),
            role: InputMessageRole::Developer,
            r#type: ResponseInputItemType::Message,
            phase: None,
        }
    }

    /// An assistant message with the given text.
    pub fn assistant(content: impl Into<ResponseInputContent>) -> Self {
        Self {
            content: content.into(),
            role: InputMessageRole::Assistant,
            r#type: ResponseInputItemType::Message,
            phase: None,
        }
    }

    /// Label the message as intermediate commentary or the final answer.
    pub fn with_phase(mut self, phase: MessagePhase) -> Self {
        self.phase = Some(phase);
        self
    }
}

/// The phase of an assistant message: intermediate commentary or final answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessagePhase {
    /// Intermediate thinking, not the final answer.
    Commentary,
    /// The final answer.
    FinalAnswer,
}

/// The kind of an input item. Tagged by `type` on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResponseInputItemType {
    /// A message input.
    #[serde(rename = "message")]
    Message,
    /// A tool result addressed by call id.
    #[serde(rename = "function_call_output")]
    FunctionCallOutput,
    /// A reference to an item by id, for replaying prior turns.
    #[serde(rename = "item_reference")]
    ItemReference,
    /// A reasoning item the model returned and the caller is replaying.
    #[serde(rename = "reasoning")]
    Reasoning,
}

/// A tool result: the function's output, addressed by the call id the model
/// gave when it made the call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionCallOutputItem {
    /// The id of the function tool call this result is responding to.
    pub call_id: String,
    /// The function's output, as a JSON-encoded string.
    pub output: String,
    /// The kind of item, always `"function_call_output"`.
    #[serde(rename = "type")]
    pub r#type: ResponseInputItemType,
    /// The item's stable id, when the endpoint assigned one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The status of the item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ItemStatus>,
}

impl FunctionCallOutputItem {
    /// A function-call result for the given call id with the given output.
    pub fn new(call_id: impl Into<String>, output: impl Into<String>) -> Self {
        Self {
            call_id: call_id.into(),
            output: output.into(),
            r#type: ResponseInputItemType::FunctionCallOutput,
            id: None,
            status: None,
        }
    }
}

/// The status of an item the model returned, populated when items are read
/// back from the API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemStatus {
    /// The model is still producing this item.
    InProgress,
    /// The item is complete.
    Completed,
    /// The item was cut off before completion.
    Incomplete,
}

/// A reference to an item by id, used to replay a prior turn's output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemReference {
    /// The id of the referenced item.
    pub id: String,
    /// The kind of item, always `"item_reference"`.
    #[serde(rename = "type")]
    pub r#type: ResponseInputItemType,
}

impl ItemReference {
    /// A reference to the item with the given id.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            r#type: ResponseInputItemType::ItemReference,
        }
    }
}

/// One item in a Responses request's `input`. The kind is the discriminant
/// on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseInputItem {
    /// A message.
    #[serde(rename = "message")]
    Message(EasyInputMessage),
    /// A tool result.
    #[serde(rename = "function_call_output")]
    FunctionCallOutput(FunctionCallOutputItem),
    /// A reference to an item by id.
    #[serde(rename = "item_reference")]
    ItemReference(ItemReference),
    /// A reasoning item the model returned on a previous turn.
    #[serde(rename = "reasoning")]
    Reasoning(ReasoningItemInput),
}

/// A reasoning item being replayed as input, so the next turn's reasoning
/// stays continuous with the previous one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReasoningItemInput {
    /// The id the endpoint assigned to this reasoning item, kept verbatim.
    pub id: String,
    /// The kind of item, always `"reasoning"`.
    #[serde(rename = "type")]
    pub r#type: ResponseInputItemType,
    /// The summary the model wrote for this reasoning block.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub summary: Vec<ReasoningSummaryText>,
    /// The reasoning text the model wrote, in arrival order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content: Vec<ReasoningTextContent>,
    /// The encrypted form of the reasoning, when the endpoint returned one.
    /// Replayed so a stateless caller can keep the chain of thought continuous.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encrypted_content: Option<String>,
}

/// A summary line the model wrote for a reasoning block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReasoningSummaryText {
    /// The summary text.
    pub text: String,
    /// The kind of summary, always `"summary_text"`.
    #[serde(rename = "type")]
    pub r#type: ReasoningTextKind,
}

/// One piece of reasoning text the model wrote.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReasoningTextContent {
    /// The reasoning text.
    pub text: String,
    /// The kind of content, always `"reasoning_text"`.
    #[serde(rename = "type")]
    pub r#type: ReasoningTextKind,
}

/// The kind of a reasoning text item: either the running text or its summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningTextKind {
    /// A summary line.
    SummaryText,
    /// A piece of running reasoning text.
    ReasoningText,
}

/// The input to a Responses request. Either a plain string — the equivalent
/// of a single user message — or a list of input items.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponseInput {
    /// A plain string input. Equivalent to a single user message.
    Text(String),
    /// A list of input items.
    Items(Vec<ResponseInputItem>),
}

impl From<&str> for ResponseInput {
    fn from(text: &str) -> Self {
        ResponseInput::Text(text.to_owned())
    }
}

impl From<String> for ResponseInput {
    fn from(text: String) -> Self {
        ResponseInput::Text(text)
    }
}

impl ResponseInput {
    /// An input from the given text.
    pub fn text(text: impl Into<String>) -> Self {
        ResponseInput::Text(text.into())
    }

    /// An input from the given items.
    pub fn items(items: Vec<ResponseInputItem>) -> Self {
        ResponseInput::Items(items)
    }
}

// ============================================================================
// Request: tools
// ============================================================================

/// A function tool the model may call. The Responses API uses a flat shape:
/// `{"type":"function","name":"...","parameters":{...}}` — unlike Chat
/// Completions, which nests the function fields under a `function` key.
/// The reference client confirms this in
/// `openai/types/responses/function_tool.py`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponseFunctionTool {
    /// The kind of tool, always `"function"`.
    #[serde(rename = "type")]
    pub r#type: ResponseToolType,
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

impl ResponseFunctionTool {
    /// A function tool with the given name and parameter schema.
    pub fn new(name: impl Into<String>, parameters: Value) -> Self {
        Self {
            r#type: ResponseToolType::Function,
            name: name.into(),
            description: None,
            parameters: Some(parameters),
            strict: None,
        }
    }

    /// Describe what the function does.
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

/// The kind of a tool on the Responses API. The reference client supports many
/// more; only `function` is modeled here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseToolType {
    /// A function the model calls with JSON arguments.
    Function,
}

/// A tool the model may call on the Responses API. Only the function form is
/// modeled here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponseTool {
    /// A function tool.
    Function(ResponseFunctionTool),
}

impl ResponseTool {
    /// A function tool with the given name and parameter schema.
    pub fn function(name: impl Into<String>, parameters: Value) -> Self {
        ResponseTool::Function(ResponseFunctionTool::new(name, parameters))
    }

    /// A function tool wrapping an existing [`ResponseFunctionTool`].
    pub fn from_function_tool(tool: ResponseFunctionTool) -> Self {
        ResponseTool::Function(tool)
    }
}

/// A specific function the model is forced to call, named in `name`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponseToolChoiceFunction {
    /// The kind of choice, always `"function"`.
    #[serde(rename = "type")]
    pub r#type: ResponseToolChoiceType,
    /// The name of the function to call.
    pub name: String,
}

impl ResponseToolChoiceFunction {
    /// Force the model to call the named function.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            r#type: ResponseToolChoiceType::Function,
            name: name.into(),
        }
    }
}

/// The kind of a `tool_choice` value: only `function` is modeled in detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResponseToolChoiceType {
    /// Force the model to call the named function.
    Function,
}

/// How the model should pick a tool (or not).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponseToolChoice {
    /// `"none"`, `"auto"`, or `"required"`.
    Mode(ResponseToolChoiceMode),
    /// Force the model to call a specific function.
    Function(ResponseToolChoiceFunction),
    /// Force the model to call one of a pre-defined set of tools.
    Allowed(ResponseToolChoiceAllowed),
    /// Force the model to use a built-in tool by kind.
    Types(ResponseToolChoiceTypes),
    /// Force the model to call a specific tool on a remote MCP server.
    Mcp(ResponseToolChoiceMcp),
    /// Force the model to call a specific custom tool.
    Custom(ResponseToolChoiceCustom),
    /// Force the model to use the programmatic-tool-calling form.
    ProgrammaticToolCalling(ProgrammaticToolCallingChoice),
    /// Force the model to use the apply-patch tool.
    ApplyPatch(ApplyPatchChoice),
    /// Force the model to use the shell tool.
    Shell(ShellChoice),
}

impl ResponseToolChoice {
    /// `"auto"` — the model may call or answer.
    pub fn auto() -> Self {
        ResponseToolChoice::Mode(ResponseToolChoiceMode::Auto)
    }

    /// `"none"` — the model will not call any tool.
    pub fn none() -> Self {
        ResponseToolChoice::Mode(ResponseToolChoiceMode::None)
    }

    /// `"required"` — the model must call one of the tools.
    pub fn required() -> Self {
        ResponseToolChoice::Mode(ResponseToolChoiceMode::Required)
    }

    /// Force the model to call the named function.
    pub fn function(name: impl Into<String>) -> Self {
        ResponseToolChoice::Function(ResponseToolChoiceFunction::new(name))
    }

    /// Constrain the model to a pre-defined set of tools. Use `mode: Required`
    /// to force the model to call one of them.
    pub fn allowed(tools: Vec<ResponseAllowedToolRef>, mode: ResponseAllowedToolMode) -> Self {
        ResponseToolChoice::Allowed(ResponseToolChoiceAllowed {
            mode,
            tools,
            r#type: ResponseToolChoiceAllowedType::AllowedTools,
        })
    }

    /// Force the model to use a specific kind of built-in tool.
    pub fn types(kind: ResponseBuiltInToolKind) -> Self {
        ResponseToolChoice::Types(ResponseToolChoiceTypes { r#type: kind })
    }

    /// Force the model to call a specific tool on a remote MCP server.
    pub fn mcp(server_label: impl Into<String>, name: Option<String>) -> Self {
        ResponseToolChoice::Mcp(ResponseToolChoiceMcp {
            server_label: server_label.into(),
            name,
            r#type: McpToolDiscriminator::Mcp,
        })
    }

    /// Force the model to call a specific custom tool.
    pub fn custom(name: impl Into<String>) -> Self {
        ResponseToolChoice::Custom(ResponseToolChoiceCustom {
            name: name.into(),
            r#type: ResponsesCustomToolDiscriminator::Custom,
        })
    }

    /// Force the model to use the programmatic-tool-calling form.
    pub fn programmatic_tool_calling() -> Self {
        ResponseToolChoice::ProgrammaticToolCalling(ProgrammaticToolCallingChoice {
            r#type: ProgrammaticToolCallingTag::ProgrammaticToolCalling,
        })
    }

    /// Force the model to use the apply-patch tool.
    pub fn apply_patch() -> Self {
        ResponseToolChoice::ApplyPatch(ApplyPatchChoice {
            r#type: ApplyPatchTag::ApplyPatch,
        })
    }

    /// Force the model to use the shell tool.
    pub fn shell() -> Self {
        ResponseToolChoice::Shell(ShellChoice {
            r#type: ShellTag::Shell,
        })
    }
}

/// Force the model to call a specific tool on a remote MCP server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponseToolChoiceMcp {
    /// The label of the MCP server to use.
    pub server_label: String,
    /// The name of the tool to call on the server.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub name: Option<String>,
    /// The kind of tool choice, always `"mcp"`.
    #[serde(rename = "type")]
    pub r#type: McpToolDiscriminator,
}

/// The tag for [`ResponseToolChoiceMcp`], always `"mcp"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum McpToolDiscriminator {
    /// The MCP tool choice.
    #[serde(rename = "mcp")]
    Mcp,
}

/// Force the model to call a specific custom tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponseToolChoiceCustom {
    /// The name of the custom tool to call.
    pub name: String,
    /// The kind of tool choice, always `"custom"`.
    #[serde(rename = "type")]
    pub r#type: ResponsesCustomToolDiscriminator,
}

/// The tag for [`ResponseToolChoiceCustom`], always `"custom"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResponsesCustomToolDiscriminator {
    /// The custom tool choice.
    #[serde(rename = "custom")]
    Custom,
}

/// A `tool_choice` of `{"type": "programmatic_tool_calling"}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProgrammaticToolCallingChoice {
    /// The kind of tool choice, always `"programmatic_tool_calling"`.
    #[serde(rename = "type")]
    pub r#type: ProgrammaticToolCallingTag,
}

/// The tag for [`ProgrammaticToolCallingChoice`], always
/// `"programmatic_tool_calling"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProgrammaticToolCallingTag {
    /// The programmatic-tool-calling form.
    #[serde(rename = "programmatic_tool_calling")]
    ProgrammaticToolCalling,
}

/// A `tool_choice` of `{"type": "apply_patch"}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApplyPatchChoice {
    /// The kind of tool choice, always `"apply_patch"`.
    #[serde(rename = "type")]
    pub r#type: ApplyPatchTag,
}

/// The tag for [`ApplyPatchChoice`], always `"apply_patch"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApplyPatchTag {
    /// The apply-patch tool.
    #[serde(rename = "apply_patch")]
    ApplyPatch,
}

/// A `tool_choice` of `{"type": "shell"}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShellChoice {
    /// The kind of tool choice, always `"shell"`.
    #[serde(rename = "type")]
    pub r#type: ShellTag,
}

/// The tag for [`ShellChoice`], always `"shell"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShellTag {
    /// The shell tool.
    #[serde(rename = "shell")]
    Shell,
}

/// The three string forms of [`ResponseToolChoice`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResponseToolChoiceMode {
    /// The model may call a tool or answer in text.
    Auto,
    /// The model must not call a tool.
    None,
    /// The model must call one of the tools.
    Required,
}

/// A reference to a tool that may be called, by kind and identifying field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseAllowedToolRef {
    /// A function tool.
    #[serde(rename = "function")]
    Function {
        /// The function's name.
        name: String,
    },
    /// A hosted built-in tool.
    #[serde(rename = "hosted_tool")]
    HostedTool {
        /// The tool's name, e.g. `"web_search"`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// A custom (user-defined) tool.
    #[serde(rename = "custom")]
    Custom {
        /// The tool's name.
        name: String,
    },
}

/// The mode that drives a [`ResponseToolChoice::Allowed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseAllowedToolMode {
    /// The model picks from the allowed tools or generates a message.
    Auto,
    /// The model must call one or more of the allowed tools.
    Required,
}

/// Constrain the model to a pre-defined set of tools.
///
/// `type` is fixed to `"allowed_tools"`; the mode and the list of allowed
/// tools are the dynamic parts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponseToolChoiceAllowed {
    /// Whether the model may or must call one of the allowed tools.
    pub mode: ResponseAllowedToolMode,
    /// The tools the model is allowed to call.
    pub tools: Vec<ResponseAllowedToolRef>,
    /// The kind of tool choice, always `"allowed_tools"`.
    #[serde(rename = "type")]
    pub r#type: ResponseToolChoiceAllowedType,
}

/// The tag for a [`ResponseToolChoiceAllowed`], always `"allowed_tools"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResponseToolChoiceAllowedType {
    /// The model picks from the allowed tools.
    #[serde(rename = "allowed_tools")]
    AllowedTools,
}

/// Force the model to use a specific kind of built-in tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponseToolChoiceTypes {
    /// The kind of built-in tool the model must use.
    #[serde(rename = "type")]
    pub r#type: ResponseBuiltInToolKind,
}

/// The kinds of built-in tools the model may be forced to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseBuiltInToolKind {
    /// File search.
    FileSearch,
    /// Web search preview.
    WebSearchPreview,
    /// Computer use.
    Computer,
    /// Computer use preview (older form).
    ComputerUsePreview,
    /// Computer use (current form).
    ComputerUse,
    /// Web search preview dated `2025-03-11`.
    #[serde(rename = "web_search_preview_2025_03_11")]
    WebSearchPreview20250311,
    /// Image generation.
    ImageGeneration,
    /// Code interpreter.
    CodeInterpreter,
}

// ============================================================================
// Request: reasoning, text, and other knobs
// ============================================================================

/// Configuration for the model's reasoning effort.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReasoningConfig {
    /// How much the model thinks before answering.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub effort: Option<ReasoningEffort>,
    /// A token cap on reasoning tokens. The model stops reasoning when it
    /// hits this limit, even if it has not finished.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub max_tokens: Option<u32>,
}

impl ReasoningConfig {
    /// A reasoning config with the given effort.
    pub fn with_effort(effort: ReasoningEffort) -> Self {
        Self {
            effort: Some(effort),
            max_tokens: None,
        }
    }

    /// A reasoning config with the given token cap.
    pub fn with_max_tokens(max_tokens: u32) -> Self {
        Self {
            effort: None,
            max_tokens: Some(max_tokens),
        }
    }
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

/// The shape of the model's text answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseTextFormat {
    /// Plain text. The default.
    #[serde(rename = "text")]
    Text,
    /// A JSON object. The model is steered toward producing valid JSON.
    #[serde(rename = "json_object")]
    JsonObject,
}

impl ResponseTextFormat {
    /// Plain text.
    pub fn text() -> Self {
        ResponseTextFormat::Text
    }

    /// A JSON object, no schema.
    pub fn json_object() -> Self {
        ResponseTextFormat::JsonObject
    }
}

/// Configuration options for the text response. Used to steer the model
/// toward JSON output, structured output, or plain text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextConfig {
    /// The format the model's text should take.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub format: Option<ResponseTextFormat>,
}

impl TextConfig {
    /// A text config with no format constraint.
    pub fn new() -> Self {
        Self { format: None }
    }

    /// Set the format.
    pub fn with_format(mut self, format: ResponseTextFormat) -> Self {
        self.format = Some(format);
        self
    }
}

impl Default for TextConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// How the request handles context that exceeds the model's window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Truncation {
    /// Drop items from the beginning to fit the window.
    Auto,
    /// Fail with a 400 when the input would overflow.
    Disabled,
}

/// The service tier the request should run on.
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

/// Set of key-value pairs the backend can attach to an object.
pub type Metadata = HashMap<String, String>;

/// Options for the streaming response. Only meaningful when `stream: true`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StreamOptions {
    /// Whether to include an obfuscation string on each delta event to
    /// normalize payload sizes. Defaults to true; set false to optimize
    /// for bandwidth when the network is trusted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_obfuscation: Option<bool>,
}

impl StreamOptions {
    /// Stream options that turn obfuscation off.
    pub fn without_obfuscation() -> Self {
        Self {
            include_obfuscation: Some(false),
        }
    }
}

// ============================================================================
// Request: top level
// ============================================================================

/// The `POST /v1/responses` body.
///
/// Built rather than assembled: `new` takes the two fields the spec requires,
/// and every other field has a `with_…` that names what it does on the wire,
/// so a request cannot carry a field nobody meant to send. Nothing here is a
/// default: an omitted field is absent from the JSON, not null, because the
/// two are not the same request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponseCreateRequest {
    /// The model id. Spec-required.
    pub model: String,
    /// The conversation, oldest first. Spec-required.
    pub input: ResponseInput,

    /// A system (or developer) message inserted into the model's context.
    /// Separate from the input items so a caller can swap them between turns
    /// without rewriting the conversation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,

    /// An upper bound on the tokens the model produces, including reasoning.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,

    /// Sampling temperature, between 0 and 2.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Nucleus sampling.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,

    /// The tools the model may call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ResponseTool>>,
    /// How the model picks from those tools.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ResponseToolChoice>,
    /// Whether the model may call more than one tool per turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,

    /// Configuration for the model's reasoning.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningConfig>,
    /// Configuration for the model's text response.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<TextConfig>,

    /// Which capacity serves the request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<ServiceTier>,
    /// Set of key-value pairs the backend can attach to the response.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,

    /// The id of the previous response, for continuing a conversation. Cannot
    /// be combined with a `conversation`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_response_id: Option<String>,

    /// How to handle a context that exceeds the model's window.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<Truncation>,

    /// A stable identifier for the end user making the request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// A stable identifier that helps OpenAI detect users violating the usage
    /// policies. Preferable to `user` for abuse detection.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub safety_identifier: Option<String>,

    /// A key OpenAI uses to bucket requests for prompt caching. Replaces the
    /// `user` field for cache purposes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,

    /// Whether the response is stored for later retrieval via the API. Defaults
    /// to true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,

    /// Whether to run the request in the background and return immediately.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background: Option<bool>,

    /// An upper bound on the total number of built-in tool calls the model
    /// may make in this response. Further calls are ignored.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tool_calls: Option<u32>,

    /// Configuration for running moderation on the request input and the
    /// generated output. When set, the endpoint runs the named moderation
    /// model on each side and returns the result on the response.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub moderation: Option<ResponseModerationConfig>,

    /// Options for prompt caching on `gpt-5.6` and later. Sent in the
    /// request body (the response side carries the resolved options).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_options: Option<crate::types::PromptCacheOptions>,

    /// How long the prompt cache should keep its entries. Deprecated in favor
    /// of [`PromptCacheOptions::ttl`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_retention: Option<crate::types::PromptCacheRetention>,

    /// An integer between 0 and 20 specifying the maximum number of most
    /// likely tokens to return at each token position, each with an
    /// associated log probability.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_logprobs: Option<u32>,

    /// Whether to answer as a stream of events. Set by [`Self::streaming`].
    pub stream: bool,
    /// Options for the streaming response, when `stream` is true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
}

impl ResponseCreateRequest {
    /// A non-streaming request for the given model and input.
    pub fn new(model: impl Into<String>, input: impl Into<ResponseInput>) -> Self {
        Self {
            model: model.into(),
            input: input.into(),
            instructions: None,
            max_output_tokens: None,
            temperature: None,
            top_p: None,
            tools: None,
            tool_choice: None,
            parallel_tool_calls: None,
            reasoning: None,
            text: None,
            service_tier: None,
            metadata: None,
            previous_response_id: None,
            truncation: None,
            user: None,
            safety_identifier: None,
            prompt_cache_key: None,
            store: None,
            background: None,
            max_tool_calls: None,
            moderation: None,
            prompt_cache_options: None,
            prompt_cache_retention: None,
            top_logprobs: None,
            stream: false,
            stream_options: None,
        }
    }

    /// A streaming request for the given model and input.
    pub fn streaming(model: impl Into<String>, input: impl Into<ResponseInput>) -> Self {
        let mut request = Self::new(model, input);
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

    /// Set `instructions`.
    pub fn with_instructions(mut self, instructions: impl Into<String>) -> Self {
        self.instructions = Some(instructions.into());
        self
    }

    /// Set `max_output_tokens`.
    pub fn with_max_output_tokens(mut self, n: u32) -> Self {
        self.max_output_tokens = Some(n);
        self
    }

    /// Set `temperature`.
    pub fn with_temperature(mut self, t: f64) -> Self {
        self.temperature = Some(t);
        self
    }

    /// Set `top_p`.
    pub fn with_top_p(mut self, top_p: f64) -> Self {
        self.top_p = Some(top_p);
        self
    }

    /// Add a tool.
    pub fn with_tool(mut self, tool: ResponseTool) -> Self {
        self.tools.get_or_insert_with(Vec::new).push(tool);
        self
    }

    /// Set `tool_choice`.
    pub fn with_tool_choice(mut self, choice: ResponseToolChoice) -> Self {
        self.tool_choice = Some(choice);
        self
    }

    /// Set `parallel_tool_calls`.
    pub fn with_parallel_tool_calls(mut self, parallel: bool) -> Self {
        self.parallel_tool_calls = Some(parallel);
        self
    }

    /// Set `reasoning`.
    pub fn with_reasoning(mut self, reasoning: ReasoningConfig) -> Self {
        self.reasoning = Some(reasoning);
        self
    }

    /// Set `text`.
    pub fn with_text(mut self, text: TextConfig) -> Self {
        self.text = Some(text);
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

    /// Set `previous_response_id`.
    pub fn with_previous_response_id(mut self, id: impl Into<String>) -> Self {
        self.previous_response_id = Some(id.into());
        self
    }

    /// Set `truncation`.
    pub fn with_truncation(mut self, truncation: Truncation) -> Self {
        self.truncation = Some(truncation);
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

    /// Set `store`.
    pub fn with_store(mut self, store: bool) -> Self {
        self.store = Some(store);
        self
    }

    /// Set `background`.
    pub fn with_background(mut self, background: bool) -> Self {
        self.background = Some(background);
        self
    }

    /// Set `max_tool_calls`.
    pub fn with_max_tool_calls(mut self, n: u32) -> Self {
        self.max_tool_calls = Some(n);
        self
    }

    /// Set `moderation`.
    pub fn with_moderation(mut self, moderation: ResponseModerationConfig) -> Self {
        self.moderation = Some(moderation);
        self
    }

    /// Set `prompt_cache_options`.
    pub fn with_prompt_cache_options(mut self, options: crate::types::PromptCacheOptions) -> Self {
        self.prompt_cache_options = Some(options);
        self
    }

    /// Set `prompt_cache_retention`.
    pub fn with_prompt_cache_retention(
        mut self,
        retention: crate::types::PromptCacheRetention,
    ) -> Self {
        self.prompt_cache_retention = Some(retention);
        self
    }

    /// Set `top_logprobs`. Requires `logprobs`-style output to be enabled.
    pub fn with_top_logprobs(mut self, n: u32) -> Self {
        self.top_logprobs = Some(n);
        self
    }
}

// ============================================================================
// Response: the full Response
// ============================================================================

/// A Responses answer the model returned in one piece.
///
/// This is what [`crate::Client::responses`] yields: the whole answer in
/// one struct, with the output items and the status.
///
/// Tolerant by construction: every field that is not strictly required by the
/// protocol is `Option` or carries a `#[serde(default)]`, so a backend that
/// returns `null` for an unused field is still a backend that reads.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Response {
    /// The endpoint's id for this response.
    pub id: String,
    /// Unix timestamp (in seconds) of when this response was created.
    pub created_at: f64,
    /// The model that produced the response.
    pub model: String,
    /// The object type, always `"response"`.
    pub object: String,
    /// An array of content items generated by the model.
    #[serde(default, deserialize_with = "deserialize_null_default_vec")]
    pub output: Vec<ResponseOutputItem>,
    /// Whether the model may run tool calls in parallel.
    #[serde(default)]
    pub parallel_tool_calls: bool,
    /// How the model picks from its tools.
    pub tool_choice: ResponseToolChoice,
    /// The tools the model was given.
    #[serde(default, deserialize_with = "deserialize_null_default_vec")]
    pub tools: Vec<ResponseTool>,
    /// The status of the response generation.
    #[serde(default)]
    pub status: Option<ResponseStatus>,
    /// An error object the endpoint returns when the model fails.
    #[serde(default)]
    pub error: Option<ResponseError>,
    /// Token usage statistics.
    #[serde(default)]
    pub usage: Option<ResponseUsage>,
    /// The metadata the endpoint attached to the response.
    #[serde(default)]
    pub metadata: Option<Metadata>,
    /// Unix timestamp of when the response completed.
    #[serde(default)]
    pub completed_at: Option<f64>,
    /// The instructions the model was given.
    #[serde(default)]
    pub instructions: Option<String>,
    /// The model's reasoning config.
    #[serde(default)]
    pub reasoning: Option<ReasoningConfig>,
    /// The model's text config.
    #[serde(default)]
    pub text: Option<TextConfig>,
    /// Sampling temperature.
    #[serde(default)]
    pub temperature: Option<f64>,
    /// Nucleus sampling.
    #[serde(default)]
    pub top_p: Option<f64>,
    /// Max output tokens cap.
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    /// The id of the previous response in this conversation, when continuing.
    #[serde(default)]
    pub previous_response_id: Option<String>,
    /// The service tier the request actually ran on.
    #[serde(default)]
    pub service_tier: Option<ResponseServiceTierOrUnknown>,
    /// Why the response is incomplete, when it is.
    #[serde(default)]
    pub incomplete_details: Option<IncompleteDetails>,
    /// Whether the request ran in the background.
    #[serde(default)]
    pub background: Option<bool>,
    /// The conversation this response was added to, when [`Self::store`] was
    /// true and a conversation was active.
    #[serde(default)]
    pub conversation: Option<Conversation>,
    /// An upper bound on built-in tool calls the model may make in this
    /// response.
    #[serde(default)]
    pub max_tool_calls: Option<u32>,
    /// Moderation results for the response input and the generated output,
    /// when the request asked for moderation.
    #[serde(default)]
    pub moderation: Option<ResponseModeration>,
    /// A reference to a prompt template, when the request used one.
    #[serde(default)]
    pub prompt: Option<Value>,
    /// Prompt-cache diagnostics for this response, when the request asked for
    /// them.
    #[serde(default)]
    pub prompt_cache_diagnostics: Option<PromptCacheDiagnostics>,
    /// A key the endpoint uses to bucket requests for prompt caching.
    #[serde(default)]
    pub prompt_cache_key: Option<String>,
    /// The prompt-caching options that were applied to this response.
    #[serde(default)]
    pub prompt_cache_options: Option<ResponsePromptCacheOptions>,
    /// How long the prompt cache should keep its entries. Deprecated in favor
    /// of [`ResponsePromptCacheOptions::ttl`].
    #[serde(default)]
    pub prompt_cache_retention: Option<crate::types::PromptCacheRetention>,
    /// A stable identifier used to help detect users of the application that
    /// may be violating the endpoint's usage policies.
    #[serde(default)]
    pub safety_identifier: Option<String>,
    /// Whether to store the response for later retrieval via the API.
    #[serde(default)]
    pub store: Option<bool>,
    /// An integer between 0 and 20 specifying the maximum number of most
    /// likely tokens to return at each token position, each with an associated
    /// log probability.
    #[serde(default)]
    pub top_logprobs: Option<u32>,
    /// How the request handled context that exceeded the model's window.
    #[serde(default)]
    pub truncation: Option<Truncation>,
    /// A stable identifier for the end user making the request.
    #[serde(default)]
    pub user: Option<String>,
}

/// The conversation this response was added to. Present when a request
/// named a conversation and the endpoint grouped the response into it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Conversation {
    /// The unique id of the conversation.
    pub id: String,
}

/// Moderation results on a Responses request that asked for moderation.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ResponseModeration {
    /// Moderation for the request input.
    pub input: ResponseModerationSide,
    /// Moderation for the generated output.
    pub output: ResponseModerationSide,
}

/// Configuration for running moderation on a Responses request. Sent in the
/// request body (the response side carries the resolved [`ResponseModeration`]
/// with the scores).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponseModerationConfig {
    /// The moderation model to use, e.g. `"omni-moderation-latest"`.
    pub model: String,
    /// The policy to apply. Defaults to scoring both input and output.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub policy: Option<ResponseModerationPolicy>,
}

impl ResponseModerationConfig {
    /// A moderation config using the named model.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            policy: None,
        }
    }

    /// Set the moderation policy.
    pub fn with_policy(mut self, policy: ResponseModerationPolicy) -> Self {
        self.policy = Some(policy);
        self
    }
}

/// The policy the moderation step applies to a Responses request's input and
/// output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponseModerationPolicy {
    /// The input policy.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub input: Option<ResponseModerationMode>,
    /// The output policy.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub output: Option<ResponseModerationMode>,
}

/// The mode a moderation step runs in on a Responses request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseModerationMode {
    /// Return the moderation score alongside the answer.
    Score,
    /// Refuse to answer when the input or output trips the policy.
    Block,
}

/// One side of a Responses moderation result: either a score or an error.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum ResponseModerationSide {
    /// The moderation result for this side, scored.
    Scored(ResponseModerationScored),
    /// The moderation step errored on this side.
    Error {
        /// The error code.
        code: String,
        /// The error message.
        message: String,
    },
}

/// A scored Responses moderation result.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ResponseModerationScored {
    /// Whether each category fired.
    pub categories: HashMap<String, bool>,
    /// The input modalities each category applies to.
    pub category_applied_input_types: HashMap<String, Vec<String>>,
    /// The score for each category.
    pub category_scores: HashMap<String, f64>,
    /// Whether any category fired.
    pub flagged: bool,
    /// The moderation model that produced this result.
    pub model: String,
}

/// Prompt-cache diagnostics on a Responses request. Tolerant: a diagnostic
/// kind this crate does not know yet is preserved as its raw JSON value on
/// [`PromptCacheDiagnostics::Unknown`].
#[derive(Debug, Clone, PartialEq)]
pub enum PromptCacheDiagnostics {
    /// The cache was missed; the request diverged from the comparison
    /// response after a number of tokens.
    CacheMiss {
        /// The estimated tokens affected after the divergence point.
        cache_missed_tokens: i64,
        /// The reason the cache could not be reused.
        reason: String,
        /// The reusable tokens in the comparison response, when one was named.
        comparison_reusable_tokens: Option<i64>,
    },
    /// The cache was hit.
    CacheHit,
    /// The named comparison response was not found.
    ComparisonResponseNotFound,
    /// Cache diagnostics are not available on this response.
    Unavailable,
    /// A diagnostic kind this crate does not know yet, kept verbatim.
    Unknown(Value),
}

impl<'de> serde::Deserialize<'de> for PromptCacheDiagnostics {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        let Some(obj) = value.as_object() else {
            return Err(serde::de::Error::custom(
                "PromptCacheDiagnostics must be a JSON object",
            ));
        };
        let kind = obj.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "cache_miss" => {
                let cache_missed_tokens = obj
                    .get("cache_missed_tokens")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| {
                        serde::de::Error::custom("cache_miss needs cache_missed_tokens")
                    })?;
                let reason = obj
                    .get("reason")
                    .and_then(Value::as_str)
                    .ok_or_else(|| serde::de::Error::custom("cache_miss needs reason"))?
                    .to_string();
                let comparison_reusable_tokens = obj
                    .get("comparison_reusable_tokens")
                    .and_then(Value::as_i64);
                Ok(PromptCacheDiagnostics::CacheMiss {
                    cache_missed_tokens,
                    reason,
                    comparison_reusable_tokens,
                })
            }
            "cache_hit" => Ok(PromptCacheDiagnostics::CacheHit),
            "comparison_response_not_found" => {
                Ok(PromptCacheDiagnostics::ComparisonResponseNotFound)
            }
            "unavailable" => Ok(PromptCacheDiagnostics::Unavailable),
            _ => Ok(PromptCacheDiagnostics::Unknown(value)),
        }
    }
}

/// The prompt-caching options that were applied to the response. Mirrors the
/// request-side [`crate::PromptCacheOptions`] with a `comparison_response_id`
/// the endpoint may fill in.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ResponsePromptCacheOptions {
    /// Whether implicit breakpoints were enabled.
    pub mode: crate::types::PromptCacheMode,
    /// The minimum lifetime of each breakpoint.
    pub ttl: crate::types::PromptCacheTtl,
    /// The id of a response used for prompt-cache diagnostics.
    #[serde(default)]
    pub comparison_response_id: Option<String>,
}

/// Why the response is incomplete.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct IncompleteDetails {
    /// The reason: max output tokens hit, the content filter, etc.
    pub reason: Option<String>,
}

/// The status of the response generation. Tolerant by construction: a status
/// this crate does not know yet is kept as its wire string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String")]
pub enum ResponseStatus {
    /// The response is complete.
    Completed,
    /// The response failed.
    Failed,
    /// The response is still being generated.
    InProgress,
    /// The response was cancelled.
    Cancelled,
    /// The response is queued for background processing.
    Queued,
    /// The response finished but is incomplete.
    Incomplete,
    /// A status this crate does not know yet.
    Unknown(String),
}

impl From<String> for ResponseStatus {
    fn from(value: String) -> Self {
        match value.as_str() {
            "completed" => ResponseStatus::Completed,
            "failed" => ResponseStatus::Failed,
            "in_progress" => ResponseStatus::InProgress,
            "cancelled" => ResponseStatus::Cancelled,
            "queued" => ResponseStatus::Queued,
            "incomplete" => ResponseStatus::Incomplete,
            _ => ResponseStatus::Unknown(value),
        }
    }
}

impl ResponseStatus {
    /// The wire string the endpoint sent.
    pub fn as_str(&self) -> &str {
        match self {
            ResponseStatus::Completed => "completed",
            ResponseStatus::Failed => "failed",
            ResponseStatus::InProgress => "in_progress",
            ResponseStatus::Cancelled => "cancelled",
            ResponseStatus::Queued => "queued",
            ResponseStatus::Incomplete => "incomplete",
            ResponseStatus::Unknown(s) => s.as_str(),
        }
    }
}

/// An error object returned with a Response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponseError {
    /// The error code.
    pub code: String,
    /// The error message.
    pub message: String,
    /// The parameter the endpoint blamed, when it blamed one.
    #[serde(default)]
    pub param: Option<String>,
}

/// Token usage statistics for a Responses request.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ResponseUsage {
    /// Tokens the model spent on the input.
    pub input_tokens: u64,
    /// Tokens the model spent on the output.
    pub output_tokens: u64,
    /// `input_tokens + output_tokens`.
    pub total_tokens: u64,
    /// A breakdown of input tokens.
    #[serde(default)]
    pub input_tokens_details: Option<InputTokensDetails>,
    /// A breakdown of output tokens.
    #[serde(default)]
    pub output_tokens_details: Option<OutputTokensDetails>,
}

/// A breakdown of input tokens, when the endpoint reports one.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct InputTokensDetails {
    /// Tokens served from cache.
    #[serde(default)]
    pub cached_tokens: Option<u64>,
    /// Tokens written to cache on this request.
    #[serde(default)]
    pub cache_write_tokens: Option<u64>,
}

/// A breakdown of output tokens, when the endpoint reports one.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct OutputTokensDetails {
    /// Reasoning tokens spent before the answer.
    #[serde(default)]
    pub reasoning_tokens: Option<u64>,
}

/// The service tier the endpoint reports on the response. Tolerant: a tier
/// this crate does not know yet is read past as `Unknown`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum ResponseServiceTierOrUnknown {
    /// A known tier.
    Known(ServiceTier),
    /// A tier this crate does not know yet.
    Unknown(String),
}

/// Deserialize a `Vec<T>` where the wire may carry `null`: a `null` reads as
/// the empty vec, the way `#[serde(default)]` would on a missing field. The
/// endpoint that fronts these types returns `null` for several `Vec` fields
/// (`tools`, `output`, the reasoning summary) when the request set none, and
/// failing on `null` would refuse a perfectly valid response.
fn deserialize_null_default_vec<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    use serde::Deserialize;
    Ok(Option::<Vec<T>>::deserialize(deserializer)?.unwrap_or_default())
}

// ============================================================================
// Response: output items
// ============================================================================

/// One output item in a [`Response`]'s `output` array.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseOutputItem {
    /// An output message.
    #[serde(rename = "message")]
    Message(ResponseOutputMessage),
    /// A function tool call the model made.
    #[serde(rename = "function_call")]
    FunctionToolCall(ResponseFunctionToolCall),
    /// A reasoning item the model produced.
    #[serde(rename = "reasoning")]
    Reasoning(ResponseReasoningItem),
}

/// An output message from the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponseOutputMessage {
    /// The id of the output message.
    pub id: String,
    /// The content of the output message.
    pub content: Vec<OutputMessageContent>,
    /// The role, always `"assistant"`.
    pub role: String,
    /// The status of the item.
    pub status: ItemStatus,
    /// Whether this is commentary or the final answer.
    #[serde(default)]
    pub phase: Option<MessagePhase>,
}

/// One part of an output message's content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputMessageContent {
    /// A text part.
    #[serde(rename = "output_text")]
    OutputText {
        /// The text the model wrote.
        text: String,
        /// Annotations on the text, when the model added some.
        #[serde(default)]
        annotations: Vec<Value>,
    },
    /// A refusal part, when the model refused.
    #[serde(rename = "refusal")]
    Refusal {
        /// The refusal message.
        refusal: String,
    },
}

/// A function tool call the model made on the Responses API.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponseFunctionToolCall {
    /// A JSON string of the arguments to pass to the function.
    pub arguments: String,
    /// The unique id of the function tool call generated by the model. Used to
    /// address the matching tool result.
    pub call_id: String,
    /// The name of the function to run.
    pub name: String,
    /// The unique id of the function tool call item, when the endpoint assigned
    /// one (for replaying it as an `item_reference` on the next turn).
    #[serde(default)]
    pub id: Option<String>,
    /// The status of the item.
    #[serde(default)]
    pub status: Option<ItemStatus>,
}

/// A reasoning item the model produced. Replayed as input on the next turn so
/// the chain of thought stays continuous.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponseReasoningItem {
    /// The unique id of the reasoning item.
    pub id: String,
    /// Summary lines the model wrote for this reasoning block.
    pub summary: Vec<ReasoningSummaryText>,
    /// The reasoning text, when the endpoint returns it on the response.
    #[serde(default)]
    pub content: Option<Vec<ReasoningTextContent>>,
    /// The encrypted form of the reasoning, when the endpoint returned one.
    #[serde(default)]
    pub encrypted_content: Option<String>,
    /// The status of the item.
    #[serde(default)]
    pub status: Option<ItemStatus>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_minimal_request_serializes_to_what_the_endpoint_reads() {
        let request = ResponseCreateRequest::new("gpt-4o", ResponseInput::text("hi"));
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(
            value,
            json!({
                "model": "gpt-4o",
                "input": "hi",
                "stream": false,
            })
        );
    }

    #[test]
    fn a_streaming_request_with_tools_and_reasoning_round_trips() {
        let request = ResponseCreateRequest::streaming("gpt-4o", ResponseInput::text("hi"))
            .with_max_output_tokens(1024)
            .with_temperature(0.7)
            .with_tool(ResponseTool::function(
                "get_weather",
                json!({"type": "object"}),
            ))
            .with_tool_choice(ResponseToolChoice::auto())
            .with_reasoning(ReasoningConfig::with_effort(ReasoningEffort::Medium))
            .with_text(TextConfig::new().with_format(ResponseTextFormat::text()));
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["stream"], json!(true));
        assert_eq!(value["max_output_tokens"], json!(1024));
        assert_eq!(value["temperature"], json!(0.7));
        assert_eq!(value["reasoning"]["effort"], json!("medium"));
        assert_eq!(value["text"]["format"]["type"], json!("text"));
        assert_eq!(value["tools"][0]["type"], json!("function"));
        // Flat shape: name sits at the top level, NOT nested under a
        // `function` key (which is the Chat Completions shape).
        assert_eq!(value["tools"][0]["name"], json!("get_weather"));
        assert_eq!(value["tool_choice"], json!("auto"));

        let back: ResponseCreateRequest = serde_json::from_value(value).unwrap();
        assert_eq!(back, request);
    }

    #[test]
    fn input_string_stays_a_string() {
        // The point: a stored text message replays byte-for-byte.
        let value = serde_json::to_value(ResponseInput::text("just text")).unwrap();
        assert_eq!(value, json!("just text"));
    }

    #[test]
    fn input_items_carry_their_type_tag() {
        let items = vec![
            ResponseInputItem::Message(EasyInputMessage::user("hi")),
            ResponseInputItem::FunctionCallOutput(FunctionCallOutputItem::new("call_1", "sunny")),
            ResponseInputItem::ItemReference(ItemReference::new("rs_1")),
        ];
        let value = serde_json::to_value(ResponseInput::items(items)).unwrap();
        assert_eq!(value[0]["type"], json!("message"));
        assert_eq!(value[0]["role"], json!("user"));
        assert_eq!(value[1]["type"], json!("function_call_output"));
        assert_eq!(value[1]["call_id"], json!("call_1"));
        assert_eq!(value[2]["type"], json!("item_reference"));
        assert_eq!(value[2]["id"], json!("rs_1"));
    }

    #[test]
    fn easy_input_message_with_images_carries_them_as_parts() {
        let msg = EasyInputMessage::user(ResponseInputContent::Parts(vec![
            ResponseInputContentPart::text("what is this?"),
            ResponseInputContentPart::image_url("data:image/png;base64,AAAA"),
        ]));
        let value = serde_json::to_value(&msg).unwrap();
        assert_eq!(value["role"], json!("user"));
        assert_eq!(value["type"], json!("message"));
        let parts = value["content"].as_array().unwrap();
        assert_eq!(parts[0]["type"], json!("input_text"));
        assert_eq!(parts[0]["text"], json!("what is this?"));
        assert_eq!(parts[1]["type"], json!("input_image"));
        assert_eq!(parts[1]["image_url"], json!("data:image/png;base64,AAAA"));
    }

    #[test]
    fn tool_choice_serializes_each_form_the_endpoint_accepts() {
        let value = serde_json::to_value(ResponseToolChoice::auto()).unwrap();
        assert_eq!(value, json!("auto"));
        let value = serde_json::to_value(ResponseToolChoice::none()).unwrap();
        assert_eq!(value, json!("none"));
        let value = serde_json::to_value(ResponseToolChoice::required()).unwrap();
        assert_eq!(value, json!("required"));
        let value = serde_json::to_value(ResponseToolChoice::function("get_weather")).unwrap();
        assert_eq!(value["type"], json!("function"));
        assert_eq!(value["name"], json!("get_weather"));
    }

    #[test]
    fn tool_choice_allowed_and_types_round_trip() {
        let allowed = ResponseToolChoice::allowed(
            vec![
                ResponseAllowedToolRef::Function {
                    name: "get_weather".into(),
                },
                ResponseAllowedToolRef::HostedTool {
                    name: Some("web_search".into()),
                },
            ],
            ResponseAllowedToolMode::Required,
        );
        let value = serde_json::to_value(&allowed).unwrap();
        assert_eq!(value["type"], json!("allowed_tools"));
        assert_eq!(value["mode"], json!("required"));
        assert_eq!(value["tools"][0]["type"], json!("function"));
        assert_eq!(value["tools"][0]["name"], json!("get_weather"));
        assert_eq!(value["tools"][1]["type"], json!("hosted_tool"));
        assert_eq!(value["tools"][1]["name"], json!("web_search"));

        let types = ResponseToolChoice::types(ResponseBuiltInToolKind::CodeInterpreter);
        let value = serde_json::to_value(&types).unwrap();
        assert_eq!(value, json!({"type": "code_interpreter"}));
    }

    #[test]
    fn tool_choice_mcp_custom_and_simple_tags_round_trip() {
        // MCP: server label and optional tool name.
        let mcp = ResponseToolChoice::mcp("deepwiki", Some("search".into()));
        let value = serde_json::to_value(&mcp).unwrap();
        assert_eq!(value["type"], json!("mcp"));
        assert_eq!(value["server_label"], json!("deepwiki"));
        assert_eq!(value["name"], json!("search"));

        // Custom: just a tool name.
        let custom = ResponseToolChoice::custom("my_tool");
        let value = serde_json::to_value(&custom).unwrap();
        assert_eq!(value["type"], json!("custom"));
        assert_eq!(value["name"], json!("my_tool"));

        // Simple tags: just `{"type": "X"}`.
        for (variant, tag) in [
            (
                ResponseToolChoice::programmatic_tool_calling(),
                "programmatic_tool_calling",
            ),
            (ResponseToolChoice::apply_patch(), "apply_patch"),
            (ResponseToolChoice::shell(), "shell"),
        ] {
            let value = serde_json::to_value(variant).unwrap();
            assert_eq!(value, json!({"type": tag}), "for tag {tag}");
        }
    }

    #[test]
    fn request_prompt_cache_options_and_retention_serialize() {
        let request = ResponseCreateRequest::new("gpt-4o", "hi")
            .with_prompt_cache_options(crate::types::PromptCacheOptions::new())
            .with_prompt_cache_retention(crate::types::PromptCacheRetention::InMemory)
            .with_max_tool_calls(10)
            .with_top_logprobs(5);
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["prompt_cache_options"]["mode"], json!("implicit"));
        assert_eq!(value["prompt_cache_retention"], json!("in_memory"));
        assert_eq!(value["max_tool_calls"], json!(10));
        assert_eq!(value["top_logprobs"], json!(5));
    }

    #[test]
    fn request_moderation_config_serializes() {
        let request = ResponseCreateRequest::new("gpt-4o", "hi").with_moderation(
            ResponseModerationConfig::new("omni-moderation-latest").with_policy(
                ResponseModerationPolicy {
                    input: Some(ResponseModerationMode::Score),
                    output: Some(ResponseModerationMode::Block),
                },
            ),
        );
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(
            value["moderation"]["model"],
            json!("omni-moderation-latest")
        );
        assert_eq!(value["moderation"]["policy"]["input"], json!("score"));
        assert_eq!(value["moderation"]["policy"]["output"], json!("block"));
    }

    #[test]
    fn response_status_decodes_leniently() {
        let status: ResponseStatus = serde_json::from_value(json!("future_status")).unwrap();
        assert_eq!(status.as_str(), "future_status");
        assert!(matches!(status, ResponseStatus::Unknown(_)));
        let status: ResponseStatus = serde_json::from_value(json!("completed")).unwrap();
        assert_eq!(status, ResponseStatus::Completed);
    }

    #[test]
    fn a_response_carries_unknown_fields_without_failing() {
        let value = json!({
            "id": "resp_1",
            "created_at": 1.0,
            "model": "gpt-4o",
            "object": "response",
            "output": [],
            "parallel_tool_calls": true,
            "tool_choice": "auto",
            "tools": [],
            "status": "completed",
            "future_field": {"something": true},
        });
        let response: Response = serde_json::from_value(value).unwrap();
        assert_eq!(response.id, "resp_1");
        assert_eq!(response.model, "gpt-4o");
        assert_eq!(response.output.len(), 0);
    }

    #[test]
    fn a_function_tool_call_output_item_carries_call_id_and_output() {
        let item = FunctionCallOutputItem::new("call_1", "sunny");
        let value = serde_json::to_value(&item).unwrap();
        assert_eq!(value["type"], json!("function_call_output"));
        assert_eq!(value["call_id"], json!("call_1"));
        assert_eq!(value["output"], json!("sunny"));
    }

    #[test]
    fn an_output_message_with_text_and_refusal_decodes() {
        let value = json!({
            "id": "msg_1",
            "role": "assistant",
            "status": "completed",
            "content": [
                {"type": "output_text", "text": "hi"},
                {"type": "refusal", "refusal": "I cannot."},
            ],
        });
        let message: ResponseOutputMessage = serde_json::from_value(value).unwrap();
        assert_eq!(message.content.len(), 2);
        match &message.content[0] {
            OutputMessageContent::OutputText { text, .. } => assert_eq!(text, "hi"),
            _ => panic!("expected output_text"),
        }
        match &message.content[1] {
            OutputMessageContent::Refusal { refusal } => assert_eq!(refusal, "I cannot."),
            _ => panic!("expected refusal"),
        }
    }

    #[test]
    fn a_function_tool_call_output_decodes() {
        let value = json!({
            "id": "fc_1",
            "call_id": "call_1",
            "name": "get_weather",
            "arguments": "{\"city\":\"sf\"}",
            "type": "function_call",
            "status": "completed",
        });
        let call: ResponseFunctionToolCall = serde_json::from_value(value).unwrap();
        assert_eq!(call.call_id, "call_1");
        assert_eq!(call.name, "get_weather");
        assert_eq!(call.arguments, "{\"city\":\"sf\"}");
        assert_eq!(call.status, Some(ItemStatus::Completed));
    }

    #[test]
    fn a_reasoning_item_decodes_with_summary_and_content() {
        let value = json!({
            "id": "rs_1",
            "type": "reasoning",
            "summary": [{"type": "summary_text", "text": "thinking..."}],
            "content": [{"type": "reasoning_text", "text": "first chunk"}],
            "encrypted_content": "ENC",
        });
        let item: ResponseReasoningItem = serde_json::from_value(value).unwrap();
        assert_eq!(item.summary.len(), 1);
        assert_eq!(item.summary[0].text, "thinking...");
        assert_eq!(item.content.as_ref().unwrap()[0].text, "first chunk");
        assert_eq!(item.encrypted_content.as_deref(), Some("ENC"));
    }

    #[test]
    fn usage_parses_with_or_without_breakdowns() {
        let usage: ResponseUsage = serde_json::from_value(json!({
            "input_tokens": 10,
            "output_tokens": 5,
            "total_tokens": 15,
        }))
        .unwrap();
        assert_eq!(usage.input_tokens, 10);
        assert!(usage.input_tokens_details.is_none());

        let usage: ResponseUsage = serde_json::from_value(json!({
            "input_tokens": 10,
            "output_tokens": 5,
            "total_tokens": 15,
            "input_tokens_details": {"cached_tokens": 8, "cache_write_tokens": 4},
            "output_tokens_details": {"reasoning_tokens": 3},
        }))
        .unwrap();
        let details = usage.input_tokens_details.unwrap();
        assert_eq!(details.cached_tokens, Some(8));
        assert_eq!(details.cache_write_tokens, Some(4));
        let details = usage.output_tokens_details.unwrap();
        assert_eq!(details.reasoning_tokens, Some(3));
    }

    #[test]
    fn response_service_tier_tolerates_unknown_values() {
        let parsed: ResponseServiceTierOrUnknown =
            serde_json::from_value(json!("new_tier")).unwrap();
        assert!(matches!(
            parsed,
            ResponseServiceTierOrUnknown::Unknown(ref s) if s == "new_tier"
        ));
        let parsed: ResponseServiceTierOrUnknown = serde_json::from_value(json!("auto")).unwrap();
        assert_eq!(
            parsed,
            ResponseServiceTierOrUnknown::Known(ServiceTier::Auto)
        );
    }

    #[test]
    fn every_easy_input_message_constructor_carries_its_role() {
        let user = EasyInputMessage::user("u");
        assert_eq!(user.role, InputMessageRole::User);
        assert!(matches!(user.content, ResponseInputContent::Text(ref s) if s == "u"));

        let system = EasyInputMessage::system("s");
        assert_eq!(system.role, InputMessageRole::System);

        let developer = EasyInputMessage::developer("d");
        assert_eq!(developer.role, InputMessageRole::Developer);

        let assistant = EasyInputMessage::assistant("a");
        assert_eq!(assistant.role, InputMessageRole::Assistant);

        // The phase label round-trips on assistant messages.
        let assistant = EasyInputMessage::assistant("a").with_phase(MessagePhase::FinalAnswer);
        let value = serde_json::to_value(assistant).unwrap();
        assert_eq!(value["phase"], json!("final_answer"));
    }

    #[test]
    fn easy_input_message_with_images_carries_them_as_parts_new() {
        let msg = EasyInputMessage::user(ResponseInputContent::Parts(vec![
            ResponseInputContentPart::text("what is this?"),
            ResponseInputContentPart::image_url("data:image/png;base64,AAAA"),
        ]));
        let value = serde_json::to_value(&msg).unwrap();
        assert_eq!(value["role"], json!("user"));
        assert_eq!(value["type"], json!("message"));
        let parts = value["content"].as_array().unwrap();
        assert_eq!(parts[0]["type"], json!("input_text"));
        assert_eq!(parts[0]["text"], json!("what is this?"));
        assert_eq!(parts[1]["type"], json!("input_image"));
        assert_eq!(parts[1]["image_url"], json!("data:image/png;base64,AAAA"));
    }

    #[test]
    fn easy_input_message_as_string_stays_a_string() {
        // The string form is read and written back as the very string it was.
        let msg = EasyInputMessage::user("just text");
        let value = serde_json::to_value(&msg).unwrap();
        assert_eq!(value["content"], json!("just text"));
    }

    #[test]
    fn response_input_content_part_text_value_skips_images() {
        let text = ResponseInputContentPart::text("hi");
        assert_eq!(text.text_value(), Some("hi"));
        let image = ResponseInputContentPart::image_url("data:image/png;base64,AAAA");
        assert!(image.text_value().is_none());
    }

    #[test]
    fn function_call_output_item_serializes_with_its_call_id() {
        let item = FunctionCallOutputItem::new("call_1", "sunny");
        let value = serde_json::to_value(&item).unwrap();
        assert_eq!(value["type"], json!("function_call_output"));
        assert_eq!(value["call_id"], json!("call_1"));
        assert_eq!(value["output"], json!("sunny"));
        let back: FunctionCallOutputItem = serde_json::from_value(value).unwrap();
        assert_eq!(back.call_id, "call_1");
        assert_eq!(back.output, "sunny");
    }

    #[test]
    fn item_reference_serializes_as_an_id_only() {
        let reference = ItemReference::new("rs_1");
        let value = serde_json::to_value(&reference).unwrap();
        assert_eq!(value, json!({"type": "item_reference", "id": "rs_1"}));
    }

    #[test]
    fn a_reasoning_item_input_round_trips_with_summary_and_content() {
        let item = ReasoningItemInput {
            id: "rs_1".into(),
            r#type: ResponseInputItemType::Reasoning,
            summary: vec![ReasoningSummaryText {
                text: "thinking...".into(),
                r#type: ReasoningTextKind::SummaryText,
            }],
            content: vec![ReasoningTextContent {
                text: "first chunk".into(),
                r#type: ReasoningTextKind::ReasoningText,
            }],
            encrypted_content: Some("ENC".into()),
        };
        let value = serde_json::to_value(&item).unwrap();
        assert_eq!(value["type"], json!("reasoning"));
        assert_eq!(value["summary"][0]["type"], json!("summary_text"));
        assert_eq!(value["content"][0]["type"], json!("reasoning_text"));
        assert_eq!(value["encrypted_content"], json!("ENC"));
        let back: ReasoningItemInput = serde_json::from_value(value).unwrap();
        assert_eq!(back.id, "rs_1");
        assert_eq!(back.summary.len(), 1);
        assert_eq!(back.content.len(), 1);
        assert_eq!(back.encrypted_content.as_deref(), Some("ENC"));
    }

    #[test]
    fn response_tool_choice_modes_serialize_as_strings() {
        let value = serde_json::to_value(ResponseToolChoice::auto()).unwrap();
        assert_eq!(value, json!("auto"));
        let value = serde_json::to_value(ResponseToolChoice::none()).unwrap();
        assert_eq!(value, json!("none"));
        let value = serde_json::to_value(ResponseToolChoice::required()).unwrap();
        assert_eq!(value, json!("required"));
        let value = serde_json::to_value(ResponseToolChoice::function("get_weather")).unwrap();
        assert_eq!(value["type"], json!("function"));
        assert_eq!(value["name"], json!("get_weather"));
    }

    #[test]
    fn reasoning_config_serializes_optional_fields() {
        let value =
            serde_json::to_value(ReasoningConfig::with_effort(ReasoningEffort::High)).unwrap();
        assert_eq!(value["effort"], json!("high"));
        assert!(value.get("max_tokens").is_none());

        let value = serde_json::to_value(ReasoningConfig::with_max_tokens(1024)).unwrap();
        assert_eq!(value["max_tokens"], json!(1024));
        assert!(value.get("effort").is_none());
    }

    #[test]
    fn text_config_serializes_optional_format() {
        let value = serde_json::to_value(TextConfig::new()).unwrap();
        assert!(value.get("format").is_none());
        let value =
            serde_json::to_value(TextConfig::new().with_format(ResponseTextFormat::json_object()))
                .unwrap();
        assert_eq!(value["format"]["type"], json!("json_object"));
    }

    #[test]
    fn every_request_builder_round_trips() {
        // The request builders the spec needs, end to end.
        let request = ResponseCreateRequest::new("gpt-4o", ResponseInput::text("hi"))
            .with_instructions("be brief")
            .with_max_output_tokens(1024)
            .with_temperature(0.7)
            .with_top_p(0.9)
            .with_parallel_tool_calls(false)
            .with_reasoning(ReasoningConfig::with_effort(ReasoningEffort::High))
            .with_text(TextConfig::new().with_format(ResponseTextFormat::text()))
            .with_service_tier(ServiceTier::Auto)
            .with_metadata([("trace_id".into(), "abc".into())].into_iter().collect())
            .with_previous_response_id("resp_prev")
            .with_truncation(Truncation::Auto)
            .with_user("user-1")
            .with_safety_identifier("safe-1")
            .with_prompt_cache_key("cache-1")
            .with_store(true)
            .with_background(false);
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["instructions"], json!("be brief"));
        assert_eq!(value["max_output_tokens"], json!(1024));
        assert_eq!(value["temperature"], json!(0.7));
        assert_eq!(value["top_p"], json!(0.9));
        assert_eq!(value["parallel_tool_calls"], json!(false));
        assert_eq!(value["reasoning"]["effort"], json!("high"));
        assert_eq!(value["text"]["format"]["type"], json!("text"));
        assert_eq!(value["service_tier"], json!("auto"));
        assert_eq!(value["metadata"]["trace_id"], json!("abc"));
        assert_eq!(value["previous_response_id"], json!("resp_prev"));
        assert_eq!(value["truncation"], json!("auto"));
        assert_eq!(value["user"], json!("user-1"));
        assert_eq!(value["safety_identifier"], json!("safe-1"));
        assert_eq!(value["prompt_cache_key"], json!("cache-1"));
        assert_eq!(value["store"], json!(true));
        assert_eq!(value["background"], json!(false));
        let back: ResponseCreateRequest = serde_json::from_value(value).unwrap();
        assert_eq!(back, request);
    }

    #[test]
    fn response_status_unknown_variants_keep_their_wire_string() {
        assert_eq!(ResponseStatus::Completed.as_str(), "completed");
        assert_eq!(ResponseStatus::Failed.as_str(), "failed");
        assert_eq!(ResponseStatus::InProgress.as_str(), "in_progress");
        assert_eq!(ResponseStatus::Cancelled.as_str(), "cancelled");
        assert_eq!(ResponseStatus::Queued.as_str(), "queued");
        assert_eq!(ResponseStatus::Incomplete.as_str(), "incomplete");
        assert_eq!(
            ResponseStatus::Unknown("future_status".into()).as_str(),
            "future_status"
        );
    }

    #[test]
    fn a_full_response_with_output_items_deserializes() {
        let value = json!({
            "id": "resp_1",
            "created_at": 1.0,
            "model": "gpt-4o",
            "object": "response",
            "output": [
                {
                    "id": "msg_1",
                    "type": "message",
                    "role": "assistant",
                    "status": "completed",
                    "content": [{"type": "output_text", "text": "hi"}],
                },
                {
                    "id": "fc_1",
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "get_weather",
                    "arguments": "{}",
                },
            ],
            "parallel_tool_calls": true,
            "tool_choice": "auto",
            "tools": [],
            "status": "completed",
            "usage": {
                "input_tokens": 10,
                "output_tokens": 5,
                "total_tokens": 15,
            },
        });
        let response: Response = serde_json::from_value(value).unwrap();
        assert_eq!(response.id, "resp_1");
        assert_eq!(response.output.len(), 2);
        match &response.output[0] {
            ResponseOutputItem::Message(msg) => assert_eq!(msg.id, "msg_1"),
            _ => panic!("expected message"),
        }
        match &response.output[1] {
            ResponseOutputItem::FunctionToolCall(call) => assert_eq!(call.name, "get_weather"),
            _ => panic!("expected function call"),
        }
        assert_eq!(response.usage.unwrap().total_tokens, 15);
    }

    #[test]
    fn stream_options_serializes_optional_obfuscation() {
        // The default for `include_obfuscation` is true; absent means default.
        let value = serde_json::to_value(StreamOptions::default()).unwrap();
        assert!(value.get("include_obfuscation").is_none());
        let value = serde_json::to_value(StreamOptions::without_obfuscation()).unwrap();
        assert_eq!(value["include_obfuscation"], json!(false));
    }

    #[test]
    fn function_tool_def_with_description_and_strict_serializes() {
        let def = ResponseFunctionTool::new("get_weather", json!({"type": "object"}))
            .with_description("Look up the weather")
            .with_strict(true);
        let value = serde_json::to_value(def).unwrap();
        assert_eq!(value["type"], json!("function"));
        assert_eq!(value["name"], json!("get_weather"));
        assert_eq!(value["description"], json!("Look up the weather"));
        assert_eq!(value["strict"], json!(true));
    }

    #[test]
    fn response_function_tool_wraps_a_function_def() {
        let tool = ResponseTool::function("get_weather", json!({"type": "object"}));
        let value = serde_json::to_value(&tool).unwrap();
        assert_eq!(value["type"], json!("function"));
        assert_eq!(value["name"], json!("get_weather"));
        // Flat shape: no nested `function` key (unlike Chat Completions).
        assert!(value.get("function").is_none());
    }

    #[test]
    fn response_tolerates_null_for_tools_and_output() {
        // A real endpoint returns `null` for several `Vec` fields when the
        // request set none. `null` should read as empty, not fail.
        let value = json!({
            "id": "resp_1",
            "created_at": 1.0,
            "model": "gpt-4o",
            "object": "response",
            "output": [{"id":"msg_1","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"hi"}]}],
            "tool_choice": "auto",
            "tools": null,
        });
        let response: Response = serde_json::from_value(value).unwrap();
        assert_eq!(response.tools.len(), 0);
        assert_eq!(response.output.len(), 1);
    }

    #[test]
    fn response_tolerates_null_output() {
        let value = json!({
            "id": "resp_1",
            "created_at": 1.0,
            "model": "gpt-4o",
            "object": "response",
            "output": null,
            "tool_choice": "auto",
            "tools": null,
        });
        let response: Response = serde_json::from_value(value).unwrap();
        assert_eq!(response.output.len(), 0);
        assert_eq!(response.tools.len(), 0);
    }

    #[test]
    fn response_carries_the_new_optional_fields() {
        let value = json!({
            "id": "resp_1",
            "created_at": 1.0,
            "model": "gpt-4o",
            "object": "response",
            "tool_choice": "auto",
            "background": true,
            "conversation": {"id": "conv_1"},
            "max_tool_calls": 10,
            "max_output_tokens": 1024,
            "previous_response_id": "resp_prev",
            "safety_identifier": "safe-1",
            "prompt_cache_key": "cache-1",
            "prompt_cache_options": {"mode": "explicit", "ttl": "30m"},
            "prompt_cache_retention": "in_memory",
            "store": false,
            "top_logprobs": 5,
            "truncation": "auto",
            "user": "user-1",
        });
        let response: Response = serde_json::from_value(value).unwrap();
        assert_eq!(response.background, Some(true));
        assert_eq!(response.conversation.unwrap().id, "conv_1");
        assert_eq!(response.max_tool_calls, Some(10));
        assert_eq!(response.max_output_tokens, Some(1024));
        assert_eq!(response.previous_response_id.as_deref(), Some("resp_prev"));
        assert_eq!(response.safety_identifier.as_deref(), Some("safe-1"));
        assert_eq!(response.prompt_cache_key.as_deref(), Some("cache-1"));
        assert!(response.prompt_cache_options.is_some());
        assert!(matches!(
            response.prompt_cache_retention,
            Some(crate::types::PromptCacheRetention::InMemory)
        ));
        assert_eq!(response.store, Some(false));
        assert_eq!(response.top_logprobs, Some(5));
        assert!(matches!(response.truncation, Some(Truncation::Auto)));
        assert_eq!(response.user.as_deref(), Some("user-1"));
    }

    #[test]
    fn prompt_cache_diagnostics_handles_each_kind() {
        let miss: PromptCacheDiagnostics = serde_json::from_value(json!({
            "type": "cache_miss",
            "cache_missed_tokens": 100,
            "reason": "tools_changed",
            "comparison_reusable_tokens": 50
        }))
        .unwrap();
        match miss {
            PromptCacheDiagnostics::CacheMiss {
                cache_missed_tokens,
                reason,
                comparison_reusable_tokens,
                ..
            } => {
                assert_eq!(cache_missed_tokens, 100);
                assert_eq!(reason, "tools_changed");
                assert_eq!(comparison_reusable_tokens, Some(50));
            }
            _ => panic!("expected CacheMiss"),
        }
        let hit: PromptCacheDiagnostics =
            serde_json::from_value(json!({"type": "cache_hit"})).unwrap();
        assert!(matches!(hit, PromptCacheDiagnostics::CacheHit));

        // An unknown diagnostic kind is preserved verbatim.
        let unknown: PromptCacheDiagnostics =
            serde_json::from_value(json!({"type": "future_kind", "x": 1})).unwrap();
        match unknown {
            PromptCacheDiagnostics::Unknown(v) => {
                assert_eq!(v["type"], json!("future_kind"));
            }
            _ => panic!("expected Unknown"),
        }
    }

    #[test]
    fn response_moderation_deserializes_a_scored_result() {
        let value = json!({
            "input": {
                "categories": {"hate": false},
                "category_applied_input_types": {"hate": ["text"]},
                "category_scores": {"hate": 0.001},
                "flagged": false,
                "model": "omni-moderation-latest"
            },
            "output": {
                "code": "rate_limit_exceeded",
                "message": "too many requests"
            }
        });
        let m: ResponseModeration = serde_json::from_value(value).unwrap();
        match m.input {
            ResponseModerationSide::Scored(s) => {
                assert!(!s.flagged);
                assert_eq!(s.model, "omni-moderation-latest");
            }
            _ => panic!("expected scored input"),
        }
        match m.output {
            ResponseModerationSide::Error { code, .. } => {
                assert_eq!(code, "rate_limit_exceeded");
            }
            _ => panic!("expected error output"),
        }
    }
}
