use serde::{Deserialize, Serialize};

// ============================================================================
// DeepSeek /chat/completions wire 类型。
// 约束：本文件里的字段名 = API 字段名（serde rename 仅用于 r#type）。
// KVCache 前缀匹配要求历史消息逐字节回放，因此 Message 的字符串字段
// 存什么发什么，禁止在发送路径上做 trim/normalize/裁剪。
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

/// thinking 开关。type: "enabled" | "disabled"，API 默认 enabled。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thinking {
    pub r#type: String,
}

impl Thinking {
    pub fn enabled() -> Self {
        Self { r#type: "enabled".into() }
    }
    pub fn disabled() -> Self {
        Self { r#type: "disabled".into() }
    }
    pub fn is_enabled(&self) -> bool {
        self.r#type == "enabled"
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

/// 消息。字段名与 API 完全一致：
/// - assistant: content / reasoning_content / tool_calls（带 tools 的请求必须回传 reasoning_content，缺失 => 400）
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
        Self { role: Role::System, content: Some(content.into()), ..Default::default() }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self { role: Role::User, content: Some(content.into()), ..Default::default() }
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
// 流式响应（SSE chunk）
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

/// usage。prompt_tokens = prompt_cache_hit_tokens + prompt_cache_miss_tokens。
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
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ChatChunk {
    #[serde(default)]
    pub choices: Vec<ChunkChoice>,
    #[serde(default)]
    pub usage: Option<Usage>,
}

// ============================================================================
// TurnAccumulator：把一轮请求的流式 delta 聚合为完整 assistant Message。
// tool_calls 按 index 分片到达：id/name 首片给出，arguments 逐片追加。
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
                            name: dtc.function.as_ref().and_then(|f| f.name.clone()).unwrap_or_default(),
                            arguments: dtc.function.and_then(|f| f.arguments).unwrap_or_default(),
                        },
                    })
                    .collect(),
            )
        };
        Message {
            role: Role::Assistant,
            content: Some(self.content),
            reasoning_content: if self.reasoning_content.is_empty() { None } else { Some(self.reasoning_content) },
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
                function: ToolCallFunction { name: "run_shell".into(), arguments: r#"{"command":"ls"}"#.into() },
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
        let mk = |content: Option<&str>, reasoning: Option<&str>, tcs: Option<Vec<DeltaToolCall>>| Delta {
            role: None,
            content: content.map(str::to_owned),
            reasoning_content: reasoning.map(str::to_owned),
            tool_calls: tcs,
        };
        let mut acc = TurnAccumulator::default();
        acc.feed(&mk(Some("9.11 "), Some("let me "), None));
        acc.feed(&mk(Some("vs 9.8"), Some("compare."), None));
        acc.feed(&mk(None, None, Some(vec![DeltaToolCall {
            index: 0,
            id: Some("call_1".into()),
            function: Some(DeltaFunctionCall { name: Some("run_shell".into()), arguments: Some("{\"comm".into()) }),
        }])));
        acc.feed(&mk(None, None, Some(vec![DeltaToolCall {
            index: 0,
            id: None,
            function: Some(DeltaFunctionCall { name: None, arguments: Some("and\":\"ls\"}".into()) }),
        }])));
        let msg = acc.finish();
        assert_eq!(msg.content.as_deref(), Some("9.11 vs 9.8"));
        assert_eq!(msg.reasoning_content.as_deref(), Some("let me compare."));
        let tcs = msg.tool_calls.unwrap();
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0].id, "call_1");
        assert_eq!(tcs[0].function.name, "run_shell");
        assert_eq!(tcs[0].function.arguments, r#"{"command":"ls"}"#);
    }

    #[test]
    fn parse_real_stream_chunk_with_reasoning() {
        let line = r#"{"id":"x","choices":[{"index":0,"delta":{"reasoning_content":"嗯"},"finish_reason":null,"logprobs":null}],"created":1,"model":"deepseek-v4-flash","object":"chat.completion.chunk"}"#;
        let chunk: ChatChunk = serde_json::from_str(line).unwrap();
        assert_eq!(chunk.choices[0].delta.as_ref().unwrap().reasoning_content.as_deref(), Some("嗯"));
    }

    #[test]
    fn parse_final_chunk_with_usage() {
        let line = r#"{"choices":[{"delta":{"content":"","finish_reason":null},"finish_reason":"stop","index":0}],"usage":{"completion_tokens":9,"prompt_tokens":17,"prompt_cache_hit_tokens":8,"prompt_cache_miss_tokens":9,"total_tokens":26}}"#;
        let chunk: ChatChunk = serde_json::from_str(line).unwrap();
        let u = chunk.usage.unwrap();
        assert_eq!(u.prompt_cache_hit_tokens, 8);
        assert_eq!(u.prompt_cache_miss_tokens, 9);
    }
}
