use serde::{Deserialize, Serialize};

// ============================================================================
// DeepSeek /chat/completions wire types.
// Constraint: field names in this file = API field names (serde rename is used
// only for r#type). KVCache prefix matching requires history messages to be
// replayed byte-for-byte, so the string fields of Message are sent exactly as
// stored: no trim/normalize/clipping on the send path.
// ============================================================================

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    #[default]
    System,
    User,
    Assistant,
    Tool,
}

/// Thinking switch. Thinking is always on; this exists only to send an explicit
/// `{"type":"enabled"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thinking {
    pub r#type: String,
}

impl Thinking {
    pub fn enabled() -> Self {
        Self {
            r#type: "enabled".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionDef {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDef {
    pub r#type: String,
    pub function: FunctionDef,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub r#type: String,
    pub function: ToolCallFunction,
}

/// One part of a message's content: a run of text, or an image. Only the fields
/// a part is using are written.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContentPart {
    pub r#type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_url: Option<ImageUrl>,
}

impl ContentPart {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            r#type: "text".into(),
            text: Some(text.into()),
            image_url: None,
        }
    }

    pub fn image(url: impl Into<String>) -> Self {
        Self {
            r#type: "image_url".into(),
            text: None,
            image_url: Some(ImageUrl { url: url.into() }),
        }
    }
}

/// Where an image part reads its image. An attached image is a `data:` URL
/// carrying the bytes themselves: nothing outside the message has to be
/// reachable for it to be sent again.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageUrl {
    pub url: String,
}

/// Message content: a plain string, or the array of parts that carries images.
///
/// Untagged, and the string tried first, so a stored text message is read and
/// written back as the very string it was: history is replayed byte-for-byte,
/// and a message that went out as `"hi"` must not come back as
/// `[{"type":"text","text":"hi"}]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Content {
    Text(String),
    Parts(Vec<ContentPart>),
}

impl Content {
    /// The message's text, whichever shape it is in: the string itself, or the
    /// text parts it carries.
    pub fn text(&self) -> String {
        match self {
            Content::Text(text) => text.clone(),
            Content::Parts(parts) => parts
                .iter()
                .filter_map(|p| p.text.as_deref())
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }

    /// The images the message carries, as the data URLs they are sent as.
    pub fn images(&self) -> Vec<&str> {
        match self {
            Content::Text(_) => Vec::new(),
            Content::Parts(parts) => parts
                .iter()
                .filter_map(|p| p.image_url.as_ref())
                .map(|image| image.url.as_str())
                .collect(),
        }
    }
}

impl From<&str> for Content {
    fn from(text: &str) -> Self {
        Content::Text(text.to_owned())
    }
}

impl From<String> for Content {
    fn from(text: String) -> Self {
        Content::Text(text)
    }
}

/// A thinking block as the Anthropic wire returned it, carried on the assistant
/// message so the next request on that wire replays it verbatim — MiniMax
/// requires the complete content (the signature included) to come back in
/// multi-turn tool dialogs, to keep the chain of thought continuous.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThinkingBlock {
    pub thinking: String,
    #[serde(default)]
    pub signature: String,
}

/// Message. Field names match the API exactly:
/// - assistant: content / reasoning_content / tool_calls (a request carrying
///   tools must send reasoning_content back; missing it means a 400)
/// - tool:      content / tool_call_id
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Content>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Anthropic-wire thinking blocks, set only for an answer that came from
    /// that wire. Never sent on the OpenAI wire: the request builder strips
    /// the field before an OpenAi-shaped request is serialized.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<Vec<ThinkingBlock>>,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: Some(Content::Text(content.into())),
            ..Default::default()
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: Some(Content::Text(content.into())),
            ..Default::default()
        }
    }
    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: Some(Content::Text(content.into())),
            tool_call_id: Some(tool_call_id.into()),
            ..Default::default()
        }
    }

    /// A user message with images attached: what the user said, and the images
    /// it is about, in the parts form a request carries images in. A message
    /// with nothing attached stays the string it has always been.
    pub fn user_with_images(text: impl Into<String>, images: Vec<String>) -> Self {
        let text = text.into();
        if images.is_empty() {
            return Self::user(text);
        }
        let mut parts = Vec::with_capacity(images.len() + 1);
        // What was asked comes first, then what it points at.
        if !text.is_empty() {
            parts.push(ContentPart::text(text));
        }
        parts.extend(images.into_iter().map(ContentPart::image));
        Self {
            role: Role::User,
            content: Some(Content::Parts(parts)),
            ..Default::default()
        }
    }

    /// The message's text, whichever shape its content is in; `None` when it
    /// carries none at all (an assistant message that only called tools).
    pub fn text(&self) -> Option<String> {
        self.content.as_ref().map(Content::text)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    /// Ceiling on a single answer, taken from the provider preset. Always sent:
    /// the backends' own default is far below what these models can emit, and a
    /// long `Write` cut off mid-file is a failure the model cannot see.
    pub max_tokens: u32,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDef>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<String>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<Thinking>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

// ============================================================================
// The wire the request is carried on.
//
// The Anthropic shapes are the `anthropic` crate's: it speaks the standard
// Messages protocol, so the request is built in its vocabulary and the JSON is
// its business. What this crate keeps is the one enum that says which of the
// two a request is — the internal history is provider-agnostic, and the request
// builder maps it onto one shape or the other.
// ============================================================================

/// The request as the wire that carries it sees it. The client refuses to
/// serialize one shape onto the other wire, so a preset and a request can
/// never disagree silently.
#[derive(Debug, Clone, PartialEq)]
pub enum WireRequest {
    OpenAi(ChatRequest),
    Anthropic(anthropic::MessagesRequest),
}

// ============================================================================
// Streaming response (SSE chunk)
// ============================================================================

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct DeltaToolCall {
    #[serde(default)]
    pub index: u32,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<DeltaFunctionCall>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct DeltaFunctionCall {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct Delta {
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub reasoning_content: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<DeltaToolCall>>,
    /// The signature that closes a thinking block on the Anthropic wire. The
    /// OpenAI wire never sends one; the field is a parse-side detail, never
    /// serialized back out.
    #[serde(default)]
    pub signature: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ChunkChoice {
    #[serde(default)]
    pub delta: Option<Delta>,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

/// GLM / OpenAI style cache detail: `usage.prompt_tokens_details.cached_tokens`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct PromptTokensDetails {
    #[serde(default)]
    pub cached_tokens: u64,
}

/// Normalized cached token counts. Provider wire-shape differences converge in
/// `Usage::cache()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheTokens {
    pub hit: u64,
    pub miss: u64,
}

/// usage. DeepSeek uses flat hit/miss fields (prompt_tokens = hit + miss) while
/// GLM / OpenAI use nested prompt_tokens_details.cached_tokens. Both shapes are
/// accepted.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
    #[serde(default)]
    pub prompt_cache_hit_tokens: u64,
    #[serde(default)]
    pub prompt_cache_miss_tokens: u64,
    #[serde(default)]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
}

impl Usage {
    /// Cache hit/miss tokens. Both wire shapes are tried; None when neither is
    /// recognized, so the caller can show `—` instead of inventing a 0%.
    pub fn cache(&self) -> Option<CacheTokens> {
        // DeepSeek flat fields: an all-miss first turn still reports
        // hit=0/miss=N, so key off miss.
        if self.prompt_cache_hit_tokens > 0 || self.prompt_cache_miss_tokens > 0 {
            return Some(CacheTokens {
                hit: self.prompt_cache_hit_tokens,
                miss: self.prompt_cache_miss_tokens,
            });
        }
        // GLM / OpenAI report only the hit count; miss is derived from
        // prompt_tokens.
        let details = self.prompt_tokens_details.as_ref()?;
        Some(CacheTokens {
            hit: details.cached_tokens,
            miss: self.prompt_tokens.saturating_sub(details.cached_tokens),
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ChatChunk {
    #[serde(default)]
    pub choices: Vec<ChunkChoice>,
    #[serde(default)]
    pub usage: Option<Usage>,
}

// ============================================================================
// TurnAccumulator: aggregates one request's streaming deltas into a complete
// assistant Message. tool_calls arrive sharded by index: id/name come in the
// first shard, arguments are appended shard by shard. A thinking block on the
// Anthropic wire ends with a signature delta; the text streamed before it is
// the block's body, and the pair is kept so the wire can replay it verbatim.
// ============================================================================

#[derive(Debug, Default)]
pub struct TurnAccumulator {
    content: String,
    reasoning_content: String,
    tool_calls: Vec<DeltaToolCall>,
    /// The body of the thinking block being streamed, not yet closed by a
    /// signature.
    thinking_buf: String,
    /// Thinking blocks closed by a signature, in arrival order.
    thinking: Vec<ThinkingBlock>,
}

impl TurnAccumulator {
    pub fn feed(&mut self, delta: &Delta) {
        if let Some(s) = &delta.content {
            self.content.push_str(s);
        }
        if let Some(s) = &delta.reasoning_content {
            self.reasoning_content.push_str(s);
            self.thinking_buf.push_str(s);
        }
        if let Some(sig) = &delta.signature {
            // Only a real signature closes a block: an empty one over a
            // streamed body is nothing to replay, and the body stays in the
            // flat reasoning field.
            if !sig.is_empty() {
                self.thinking.push(ThinkingBlock {
                    thinking: std::mem::take(&mut self.thinking_buf),
                    signature: sig.clone(),
                });
            }
        }
        if let Some(tcs) = &delta.tool_calls {
            for dtc in tcs {
                let idx = dtc.index as usize;
                match self.tool_calls.get_mut(idx) {
                    Some(existing) => {
                        if let Some(id) = &dtc.id {
                            existing.id = Some(id.clone());
                        }
                        if let Some(f) = &dtc.function {
                            let ef = existing.function.get_or_insert_with(Default::default);
                            if let Some(n) = &f.name {
                                ef.name = Some(n.clone());
                            }
                            if let Some(a) = &f.arguments {
                                ef.arguments.get_or_insert_with(String::new).push_str(a);
                            }
                        }
                    }
                    None => {
                        while self.tool_calls.len() < idx {
                            self.tool_calls.push(DeltaToolCall::default());
                        }
                        self.tool_calls.push(dtc.clone());
                    }
                }
            }
        }
    }

    pub fn finish(self) -> Message {
        let tool_calls = if self.tool_calls.is_empty() {
            None
        } else {
            Some(
                self.tool_calls
                    .into_iter()
                    .map(|dtc| ToolCall {
                        id: dtc.id.unwrap_or_default(),
                        r#type: "function".into(),
                        function: ToolCallFunction {
                            name: dtc
                                .function
                                .as_ref()
                                .and_then(|f| f.name.clone())
                                .unwrap_or_default(),
                            arguments: dtc.function.and_then(|f| f.arguments).unwrap_or_default(),
                        },
                    })
                    .collect(),
            )
        };
        // A thinking body no signature ever closed stays out of the blocks:
        // without its signature it is not the content the model returned,
        // and replaying a made-up block would be worse than replaying none.
        // The text itself is kept in `reasoning_content` either way.
        let thinking = if self.thinking.is_empty() {
            None
        } else {
            Some(self.thinking)
        };
        Message {
            role: Role::Assistant,
            content: Some(Content::Text(self.content)),
            reasoning_content: if self.reasoning_content.is_empty() {
                None
            } else {
                Some(self.reasoning_content)
            },
            tool_calls,
            tool_call_id: None,
            thinking,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assistant_message_serde_roundtrip() {
        let msg = Message {
            role: Role::Assistant,
            content: Some("answer".into()),
            reasoning_content: Some("thinking...".into()),
            tool_calls: Some(vec![ToolCall {
                id: "call_1".into(),
                r#type: "function".into(),
                function: ToolCallFunction {
                    name: "Bash".into(),
                    arguments: r#"{"command":"ls"}"#.into(),
                },
            }]),
            tool_call_id: None,
            thinking: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""reasoning_content":"thinking...""#));
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn thinking_blocks_round_trip_and_old_logs_read_on() {
        // A message that carries Anthropic thinking blocks keeps them whole,
        // signature included: the wire demands them back verbatim.
        let msg = Message {
            role: Role::Assistant,
            content: Some("done".into()),
            reasoning_content: Some("reasoned".into()),
            tool_calls: None,
            tool_call_id: None,
            thinking: Some(vec![ThinkingBlock {
                thinking: "reasoned".into(),
                signature: "cafe".into(),
            }]),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""thinking":[{"thinking":"reasoned","signature":"cafe"}]"#));
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, back);
        // A log line written before the field existed reads back as no blocks.
        let old: Message = serde_json::from_str(r#"{"role":"assistant","content":"hi"}"#).unwrap();
        assert_eq!(old.thinking, None);
    }

    #[test]
    fn message_none_fields_omitted() {
        let msg = Message::user("hi");
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(json, r#"{"role":"user","content":"hi"}"#);
    }

    #[test]
    fn a_text_content_is_written_back_as_the_string_it_was() {
        // The prefix cache matches on bytes: a message that went out as a string
        // must not come back from the log as the parts form of the same thing.
        let stored = r#"{"role":"user","content":"hi"}"#;
        let msg: Message = serde_json::from_str(stored).unwrap();
        assert_eq!(msg.content, Some(Content::Text("hi".into())));
        assert_eq!(serde_json::to_string(&msg).unwrap(), stored);
    }

    #[test]
    fn an_image_message_round_trips_as_parts() {
        let msg = Message::user_with_images(
            "what is this?",
            vec!["data:image/png;base64,Zm9vYmFy".into()],
        );
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(
            json,
            r#"{"role":"user","content":[{"type":"text","text":"what is this?"},{"type":"image_url","image_url":{"url":"data:image/png;base64,Zm9vYmFy"}}]}"#
        );
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(back, msg);
        assert_eq!(back.text().as_deref(), Some("what is this?"));
        assert_eq!(back.content.as_ref().unwrap().images().len(), 1);
    }

    #[test]
    fn an_image_message_with_no_text_carries_no_empty_text_part() {
        let msg = Message::user_with_images("", vec!["data:image/png;base64,Zm9v".into()]);
        let json = serde_json::to_value(&msg).unwrap();
        let parts = json["content"].as_array().unwrap();
        assert_eq!(parts.len(), 1, "the image is all there is: {parts:?}");
        assert_eq!(parts[0]["type"], "image_url");
    }

    #[test]
    fn a_message_with_nothing_attached_is_the_plain_string_it_was() {
        assert_eq!(
            Message::user_with_images("hi", Vec::new()),
            Message::user("hi"),
            "a message with no image must not become an empty parts array, \
             which the backend rejects"
        );
        assert_eq!(
            Message::user_with_images("hi", Vec::new())
                .text()
                .as_deref(),
            Some("hi")
        );
    }

    #[test]
    fn content_text_joins_more_than_one_text_part() {
        let content = Content::Parts(vec![
            ContentPart::image("data:image/png;base64,Zm9v"),
            ContentPart::text("first"),
            ContentPart::text("second"),
        ]);
        assert_eq!(content.text(), "first\nsecond");
    }

    #[test]
    fn a_message_with_no_content_has_no_text() {
        let msg = Message {
            role: Role::Assistant,
            ..Default::default()
        };
        assert_eq!(msg.text(), None);
        assert_eq!(Message::user("").text().as_deref(), Some(""));
    }

    #[test]
    fn request_serializes_thinking_and_effort() {
        let req = ChatRequest {
            model: "deepseek-v4-flash".into(),
            max_tokens: 384_000,
            messages: vec![Message::user("hi")],
            tools: None,
            tool_choice: None,
            stream: true,
            thinking: Some(Thinking::enabled()),
            reasoning_effort: Some("high".into()),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""thinking":{"type":"enabled"}"#));
        assert!(json.contains(r#""reasoning_effort":"high""#));
        assert!(json.contains(r#""stream":true"#));
    }

    #[test]
    fn accumulator_aggregates_split_deltas() {
        let mk = |content: Option<&str>,
                  reasoning: Option<&str>,
                  tcs: Option<Vec<DeltaToolCall>>| Delta {
            role: None,
            content: content.map(str::to_owned),
            reasoning_content: reasoning.map(str::to_owned),
            tool_calls: tcs,
            signature: None,
        };
        let mut acc = TurnAccumulator::default();
        acc.feed(&mk(Some("9.11 "), Some("let me "), None));
        acc.feed(&mk(Some("vs 9.8"), Some("compare."), None));
        acc.feed(&mk(
            None,
            None,
            Some(vec![DeltaToolCall {
                index: 0,
                id: Some("call_1".into()),
                function: Some(DeltaFunctionCall {
                    name: Some("Bash".into()),
                    arguments: Some("{\"comm".into()),
                }),
            }]),
        ));
        acc.feed(&mk(
            None,
            None,
            Some(vec![DeltaToolCall {
                index: 0,
                id: None,
                function: Some(DeltaFunctionCall {
                    name: None,
                    arguments: Some("and\":\"ls\"}".into()),
                }),
            }]),
        ));
        let msg = acc.finish();
        assert_eq!(msg.text().as_deref(), Some("9.11 vs 9.8"));
        assert_eq!(msg.reasoning_content.as_deref(), Some("let me compare."));
        let tcs = msg.tool_calls.unwrap();
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0].id, "call_1");
        assert_eq!(tcs[0].function.name, "Bash");
        assert_eq!(tcs[0].function.arguments, r#"{"command":"ls"}"#);
    }

    #[test]
    fn accumulator_fills_gaps_and_updates_existing_tool_calls() {
        let mk = |tcs: Vec<DeltaToolCall>| Delta {
            role: None,
            content: None,
            reasoning_content: None,
            tool_calls: Some(tcs),
            signature: None,
        };
        let dtc =
            |index: u32, id: Option<&str>, name: Option<&str>, args: Option<&str>| DeltaToolCall {
                index,
                id: id.map(str::to_owned),
                function: Some(DeltaFunctionCall {
                    name: name.map(str::to_owned),
                    arguments: args.map(str::to_owned),
                }),
            };
        let mut acc = TurnAccumulator::default();
        // The first shard lands directly on index=2: 0/1 are padded with
        // placeholders (the gap branch)
        acc.feed(&mk(vec![dtc(
            2,
            Some("call_a"),
            Some("read"),
            Some("{\"file"),
        )]));
        // A later shard carries the full id/name plus appended arguments (the
        // update-existing-entry branch)
        acc.feed(&mk(vec![dtc(
            2,
            Some("call_a"),
            Some("read"),
            Some("_path\":\"a.txt\"}"),
        )]));
        // Placeholder 0 is filled in afterwards, and repeating an id updates
        // rather than appending a new entry
        acc.feed(&mk(vec![dtc(0, Some("call_z"), Some("Bash"), Some("{}"))]));
        let tcs = acc.finish().tool_calls.unwrap();
        assert_eq!(tcs.len(), 3);
        assert_eq!(tcs[0].id, "call_z");
        assert_eq!(tcs[1].id, ""); // pure placeholder
        assert_eq!(tcs[2].id, "call_a");
        assert_eq!(tcs[2].function.name, "read");
        assert_eq!(tcs[2].function.arguments, r#"{"file_path":"a.txt"}"#);
    }

    #[test]
    fn accumulator_keeps_thinking_blocks_their_signatures_closed() {
        let delta = |reasoning: Option<&str>, signature: Option<&str>| Delta {
            role: None,
            content: None,
            reasoning_content: reasoning.map(str::to_owned),
            tool_calls: None,
            signature: signature.map(str::to_owned),
        };
        let mut acc = TurnAccumulator::default();
        // First thinking block: streamed, then closed by a signature.
        acc.feed(&delta(Some("step one."), None));
        acc.feed(&delta(Some(" step two."), Some("sig-1")));
        // A second block opens: the buffer restarted, and closes signed too.
        acc.feed(&delta(Some("second block."), Some("sig-2")));
        let msg = acc.finish();
        let blocks = msg.thinking.unwrap();
        assert_eq!(
            blocks,
            vec![
                ThinkingBlock {
                    thinking: "step one. step two.".into(),
                    signature: "sig-1".into()
                },
                ThinkingBlock {
                    thinking: "second block.".into(),
                    signature: "sig-2".into()
                },
            ]
        );
        // The flat field keeps the whole reasoning for whoever reads text.
        assert_eq!(
            msg.reasoning_content.as_deref(),
            Some("step one. step two.second block.")
        );
    }

    #[test]
    fn accumulator_never_builds_a_block_without_its_signature() {
        let delta = |reasoning: Option<&str>, signature: Option<&str>| Delta {
            role: None,
            content: None,
            reasoning_content: reasoning.map(str::to_owned),
            tool_calls: None,
            signature: signature.map(str::to_owned),
        };
        // A body no signature closes (a truncated stream, or the DeepSeek
        // shape where no signature exists at all) stays out of the blocks;
        // the flat reasoning field keeps the text either way.
        let mut acc = TurnAccumulator::default();
        acc.feed(&delta(Some("unsealed thought"), None));
        acc.feed(&delta(None, Some("")));
        let msg = acc.finish();
        assert_eq!(msg.thinking, None);
        assert_eq!(msg.reasoning_content.as_deref(), Some("unsealed thought"));

        // The DeepSeek shape: reasoning without any signature at all, so no
        // block is ever produced and the message is what it always was.
        let mut acc = TurnAccumulator::default();
        acc.feed(&delta(Some("plain reasoning"), None));
        let msg = acc.finish();
        assert_eq!(msg.thinking, None);
        assert_eq!(msg.reasoning_content.as_deref(), Some("plain reasoning"));
    }

    #[test]
    fn the_wire_enum_carries_the_sdks_own_request() {
        // The Anthropic shape is the `anthropic` crate's, and its JSON is that
        // crate's business — what this crate owes is the enum that says which
        // of the two shapes a request is, and the shape it hands over.
        let request = WireRequest::Anthropic(
            anthropic::MessagesRequest::new("MiniMax-M3", 131_072, vec![])
                .streaming()
                .with_system("sys"),
        );
        match &request {
            WireRequest::Anthropic(built) => {
                assert_eq!(built.model, "MiniMax-M3");
                assert_eq!(built.max_tokens, 131_072);
                assert_eq!(built.stream, Some(true));
            }
            WireRequest::OpenAi(_) => panic!("expected the Anthropic shape"),
        }
    }

    #[test]
    fn parse_real_stream_chunk_with_reasoning() {
        let line = r#"{"id":"x","choices":[{"index":0,"delta":{"reasoning_content":"thinking…"},"finish_reason":null,"logprobs":null}],"created":1,"model":"deepseek-v4-flash","object":"chat.completion.chunk"}"#;
        let chunk: ChatChunk = serde_json::from_str(line).unwrap();
        assert_eq!(
            chunk.choices[0]
                .delta
                .as_ref()
                .unwrap()
                .reasoning_content
                .as_deref(),
            Some("thinking…")
        );
    }

    #[test]
    fn parse_final_chunk_with_usage() {
        let line = r#"{"choices":[{"delta":{"content":"","finish_reason":null},"finish_reason":"stop","index":0}],"usage":{"completion_tokens":9,"prompt_tokens":17,"prompt_cache_hit_tokens":8,"prompt_cache_miss_tokens":9,"total_tokens":26}}"#;
        let chunk: ChatChunk = serde_json::from_str(line).unwrap();
        let u = chunk.usage.unwrap();
        assert_eq!(u.prompt_cache_hit_tokens, 8);
        assert_eq!(u.prompt_cache_miss_tokens, 9);
    }

    #[test]
    fn cache_reads_deepseek_flat_fields() {
        let u = Usage {
            prompt_tokens: 17,
            prompt_cache_hit_tokens: 8,
            prompt_cache_miss_tokens: 9,
            ..Default::default()
        };
        assert_eq!(u.cache(), Some(CacheTokens { hit: 8, miss: 9 }));
    }

    #[test]
    fn cache_reads_deepseek_all_miss() {
        // All-miss first turn: hit=0 but miss>0, which must not be mistaken for
        // "no cache information".
        let u = Usage {
            prompt_tokens: 17,
            prompt_cache_hit_tokens: 0,
            prompt_cache_miss_tokens: 17,
            ..Default::default()
        };
        assert_eq!(u.cache(), Some(CacheTokens { hit: 0, miss: 17 }));
    }

    #[test]
    fn cache_reads_glm_nested_details() {
        // GLM reports only cached_tokens; miss is derived from prompt_tokens.
        let line = r#"{"choices":[],"usage":{"prompt_tokens":1200,"completion_tokens":300,"total_tokens":1500,"prompt_tokens_details":{"cached_tokens":800}}}"#;
        let chunk: ChatChunk = serde_json::from_str(line).unwrap();
        let u = chunk.usage.unwrap();
        assert_eq!(
            u.cache(),
            Some(CacheTokens {
                hit: 800,
                miss: 400
            })
        );
    }

    #[test]
    fn cache_reads_glm_all_miss() {
        let u = Usage {
            prompt_tokens: 1200,
            prompt_tokens_details: Some(PromptTokensDetails { cached_tokens: 0 }),
            ..Default::default()
        };
        assert_eq!(u.cache(), Some(CacheTokens { hit: 0, miss: 1200 }));
    }

    #[test]
    fn cache_is_none_without_any_cache_field() {
        let u = Usage {
            prompt_tokens: 100,
            ..Default::default()
        };
        assert_eq!(u.cache(), None);
    }
}
