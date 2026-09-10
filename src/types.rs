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

/// Message. Field names match the API exactly:
/// - assistant: content / reasoning_content / tool_calls (a request carrying
///   tools must send reasoning_content back; missing it means a 400)
/// - tool:      content / tool_call_id
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: Some(content.into()),
            ..Default::default()
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: Some(content.into()),
            ..Default::default()
        }
    }
    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: Some(content.into()),
            tool_call_id: Some(tool_call_id.into()),
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatRequest {
    pub model: String,
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
// first shard, arguments are appended shard by shard.
// ============================================================================

#[derive(Debug, Default)]
pub struct TurnAccumulator {
    content: String,
    reasoning_content: String,
    tool_calls: Vec<DeltaToolCall>,
}

impl TurnAccumulator {
    pub fn feed(&mut self, delta: &Delta) {
        if let Some(s) = &delta.content {
            self.content.push_str(s);
        }
        if let Some(s) = &delta.reasoning_content {
            self.reasoning_content.push_str(s);
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
        Message {
            role: Role::Assistant,
            content: Some(self.content),
            reasoning_content: if self.reasoning_content.is_empty() {
                None
            } else {
                Some(self.reasoning_content)
            },
            tool_calls,
            tool_call_id: None,
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
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""reasoning_content":"thinking...""#));
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn message_none_fields_omitted() {
        let msg = Message::user("hi");
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(json, r#"{"role":"user","content":"hi"}"#);
    }

    #[test]
    fn request_serializes_thinking_and_effort() {
        let req = ChatRequest {
            model: "deepseek-v4-flash".into(),
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
        assert_eq!(msg.content.as_deref(), Some("9.11 vs 9.8"));
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
