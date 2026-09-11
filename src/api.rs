use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use std::collections::HashMap;
use std::pin::Pin;
use std::time::Duration;

use crate::provider::{Provider, Wire};
use crate::types::{ChatChunk, Delta, DeltaFunctionCall, DeltaToolCall, Usage, WireRequest};

// ============================================================================
// HTTP client + SSE streaming parsing.
// parse_sse_line / take_line are pure functions and unit-testable;
// SseStream::next_chunk yields one chunk at a time.
// Streaming contract (OpenAI wire): delta.reasoning_content precedes
// delta.content; `data: [DONE]` ends the stream; usage rides on the last
// content block (there is no standalone usage block).
// Streaming contract (Anthropic wire): every payload is a data line carrying
// a typed event; the Transcoder folds the events into the same deltas the
// accumulator reads, and the usage of message_start and message_delta is
// merged so it is emitted once, complete.
// ============================================================================

type ByteStream = Pin<Box<dyn futures_util::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>;

pub struct Client {
    http: reqwest::Client,
    api_key: String,
    url: String,
    wire: Wire,
}

impl Client {
    pub fn new(api_key: String, url: String, wire: Wire) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .build()
            .context("failed to build HTTP client")?;
        Ok(Self {
            http,
            api_key,
            url,
            wire,
        })
    }

    /// The client for a provider: its endpoint, key and wire, paired in the
    /// one place that knows all three, so a caller cannot send one provider's
    /// key (or one wire's request shape) to another provider's URL.
    pub fn for_provider(provider: &Provider, key: String) -> Result<Self> {
        Self::new(key, provider.url.to_string(), provider.wire)
    }

    /// The request bytes as the client's wire wants them. A shape handed to
    /// the other wire's endpoint is refused here rather than serialized
    /// across, which is the one misuse the WireRequest enum cannot prevent.
    fn body(&self, req: &WireRequest) -> Result<Vec<u8>> {
        match (self.wire, req) {
            (Wire::OpenAi, WireRequest::OpenAi(r)) => Ok(serde_json::to_vec(r)?),
            (Wire::Anthropic, WireRequest::Anthropic(r)) => Ok(serde_json::to_vec(r)?),
            _ => bail!("request shape does not match the provider's wire protocol"),
        }
    }

    pub async fn stream_chat(&self, req: &WireRequest) -> Result<SseStream> {
        let resp = self
            .http
            .post(&self.url)
            .bearer_auth(&self.api_key)
            .header("Content-Type", "application/json")
            .body(self.body(req)?)
            .send()
            .await
            .context("request failed (network error)")?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("API returned HTTP {status}\nresponse body: {body}");
        }
        Ok(SseStream {
            inner: Box::pin(resp.bytes_stream()),
            buf: Vec::new(),
            done: false,
            wire: self.wire,
            transcoder: Transcoder::default(),
        })
    }
}

pub struct SseStream {
    inner: ByteStream,
    buf: Vec<u8>,
    done: bool,
    wire: Wire,
    transcoder: Transcoder,
}

impl SseStream {
    /// None means the stream is over (the wire's end — `[DONE]` on the OpenAI
    /// shape, message_stop or connection close on the Anthropic one — was
    /// reached or the connection closed).
    pub async fn next_chunk(&mut self) -> Result<Option<ChatChunk>> {
        loop {
            if let Some(line) = take_line(&mut self.buf) {
                match self.wire {
                    Wire::OpenAi => match parse_sse_line(&line)? {
                        SseLine::Chunk(c) => return Ok(Some(c)),
                        SseLine::Done => {
                            self.done = true;
                            return Ok(None);
                        }
                        SseLine::Ignored => continue,
                    },
                    // On this wire even the event *name* lines are ignored:
                    // every payload is a data line, and the transcoder reads
                    // the type off the JSON itself. An event that folds into
                    // no delta (a ping, a block boundary) continues the loop
                    // rather than yielding an empty chunk.
                    Wire::Anthropic => {
                        if let Some(data) = data_of(&line)
                            && let Some(chunk) = self.transcoder.event(data)?
                        {
                            return Ok(Some(chunk));
                        }
                        continue;
                    }
                }
            }
            if self.done {
                return Ok(None);
            }
            match self.inner.next().await {
                Some(Ok(bytes)) => self.buf.extend_from_slice(&bytes),
                Some(Err(e)) => bail!("stream read error: {e}"),
                None => self.done = true,
            }
        }
    }
}

/// Pull one line out of the buffer (returns as soon as \n is seen), handling
/// \r\n. Returns None while there is no complete line.
fn take_line(buf: &mut Vec<u8>) -> Option<String> {
    let pos = buf.iter().position(|&b| b == b'\n')?;
    let line: Vec<u8> = buf.drain(..=pos).collect();
    let mut s = String::from_utf8_lossy(&line[..line.len() - 1]).into_owned();
    if s.ends_with('\r') {
        s.pop();
    }
    Some(s)
}

pub enum SseLine {
    Chunk(ChatChunk),
    Done,
    Ignored,
}

/// The payload of a data line: `data: x` and `data:x` both yield `x`; any
/// other line (blank, comment, `event:`) yields None.
fn data_of(line: &str) -> Option<&str> {
    let data = line.strip_prefix("data:")?;
    Some(data.strip_prefix(' ').unwrap_or(data))
}

/// Parse a single OpenAI-wire SSE line. Only "data:" lines are handled;
/// [DONE] ends the stream; everything else (blank lines, comments, event:
/// fields) is ignored.
fn parse_sse_line(line: &str) -> Result<SseLine> {
    let Some(data) = data_of(line) else {
        return Ok(SseLine::Ignored);
    };
    if data.trim() == "[DONE]" {
        return Ok(SseLine::Done);
    }
    if data.trim().is_empty() {
        return Ok(SseLine::Ignored);
    }
    let chunk: ChatChunk =
        serde_json::from_str(data).with_context(|| format!("failed to parse SSE chunk: {data}"))?;
    Ok(SseLine::Chunk(chunk))
}

// ============================================================================
// The Anthropic transcoder: event stream in, deltas out.
//
// The two wires carry one conversation. A text_delta, a thinking_delta and an
// input_json_delta shard mean exactly what delta.content,
// delta.reasoning_content and a sharded tool_call mean on the OpenAI wire, so
// they are translated and handed to the same accumulator; nothing downstream
// learns that a second protocol exists. The pieces the OpenAI wire does not
// have — the signature that closes a thinking block, the usage split across
// message_start and message_delta — ride the fields added for them.
// ============================================================================

#[derive(Default)]
struct Transcoder {
    /// The tool_use blocks declared so far, keyed by *content-block* index —
    /// which is not the tool-call ordinal: thinking and text blocks take
    /// indexes too. The value is the block's ordinal (the order among
    /// tool_use blocks), which is what the accumulator shards calls by; the
    /// id and name the start event carried went out with its delta.
    tool_uses: HashMap<u32, u32>,
    /// The ordinal the next tool_use block gets.
    next_tool: u32,
    /// What message_start reported about the input; emitted merged with
    /// message_delta's output count, so usage lands once, complete.
    input_usage: StreamUsage,
}

#[derive(Default, Clone, Copy)]
struct StreamUsage {
    input_tokens: u64,
    cache_read: u64,
    cache_creation: u64,
}

impl Transcoder {
    /// Fold one data line's event. None: nothing to hand downstream.
    fn event(&mut self, data: &str) -> Result<Option<ChatChunk>> {
        let ev: serde_json::Value = serde_json::from_str(data)
            .with_context(|| format!("failed to parse SSE event: {data}"))?;
        match ev.get("type").and_then(|t| t.as_str()) {
            Some("message_start") => {
                if let Some(u) = ev.pointer("/message/usage") {
                    self.input_usage = StreamUsage {
                        input_tokens: u.get("input_tokens").and_then(as_u64).unwrap_or(0),
                        cache_read: u
                            .get("cache_read_input_tokens")
                            .and_then(as_u64)
                            .unwrap_or(0),
                        cache_creation: u
                            .get("cache_creation_input_tokens")
                            .and_then(as_u64)
                            .unwrap_or(0),
                    };
                }
                Ok(None)
            }
            Some("content_block_start") => {
                let idx = block_index(&ev);
                let block = &ev["content_block"];
                if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                    // The accumulator shards tool calls by *ordinal* among
                    // tool calls, not by content-block index, so the block is
                    // given the next ordinal here and the argument shards
                    // that follow are translated onto it.
                    let ordinal = self.next_tool;
                    self.next_tool += 1;
                    let id = str_of(block, "id");
                    let name = str_of(block, "name");
                    self.tool_uses.insert(idx, ordinal);
                    return Ok(Some(chunk_with(Delta {
                        tool_calls: Some(vec![DeltaToolCall {
                            index: ordinal,
                            id: Some(id),
                            function: Some(DeltaFunctionCall {
                                name: Some(name),
                                arguments: None,
                            }),
                        }]),
                        ..Default::default()
                    })));
                }
                Ok(None)
            }
            Some("content_block_delta") => {
                let idx = block_index(&ev);
                let delta = &ev["delta"];
                let d = match delta.get("type").and_then(|t| t.as_str()) {
                    Some("text_delta") => Delta {
                        content: Some(str_of(delta, "text")),
                        ..Default::default()
                    },
                    Some("thinking_delta") => Delta {
                        reasoning_content: Some(str_of(delta, "thinking")),
                        ..Default::default()
                    },
                    Some("signature_delta") => Delta {
                        signature: Some(str_of(delta, "signature")),
                        ..Default::default()
                    },
                    Some("input_json_delta") => {
                        // The shard lands on the call its block declared;
                        // a shard for a block nobody started has nowhere
                        // to go and is dropped.
                        let Some(&ordinal) = self.tool_uses.get(&idx) else {
                            return Ok(None);
                        };
                        Delta {
                            tool_calls: Some(vec![DeltaToolCall {
                                index: ordinal,
                                id: None,
                                function: Some(DeltaFunctionCall {
                                    name: None,
                                    arguments: Some(str_of(delta, "partial_json")),
                                }),
                            }]),
                            ..Default::default()
                        }
                    }
                    // A delta kind nobody declared: nothing to fold, no reason
                    // to fail a stream that is still speaking its own language.
                    _ => return Ok(None),
                };
                Ok(Some(chunk_with(d)))
            }
            Some("message_delta") => {
                // The last event that carries anything: the output count so
                // far, merged with what message_start said about the input.
                // Cache mapping, DeepSeek's spelling: hit = tokens served
                // from cache, miss = the rest of the prompt (this turn's
                // uncached input and what it wrote into the cache).
                let output = ev
                    .pointer("/usage/output_tokens")
                    .and_then(as_u64)
                    .unwrap_or(0);
                let u = self.input_usage;
                Ok(Some(ChatChunk {
                    choices: Vec::new(),
                    usage: Some(Usage {
                        prompt_tokens: u.input_tokens + u.cache_read + u.cache_creation,
                        completion_tokens: output,
                        prompt_cache_hit_tokens: u.cache_read,
                        prompt_cache_miss_tokens: u.input_tokens + u.cache_creation,
                        ..Default::default()
                    }),
                }))
            }
            // A stream that reports its own failure is a failure: the
            // accumulator would otherwise finish an empty answer for a turn
            // that never happened.
            Some("error") => bail!("stream error event: {data}"),
            // message_stop, ping, and anything new: nothing to fold.
            _ => Ok(None),
        }
    }
}

fn as_u64(v: &serde_json::Value) -> Option<u64> {
    v.as_u64()
}

fn str_of(v: &serde_json::Value, key: &str) -> String {
    v.get(key)
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string()
}

fn block_index(ev: &serde_json::Value) -> u32 {
    ev.get("index").and_then(as_u64).unwrap_or(0) as u32
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::MINIMAX;
    use crate::types::{ChatRequest, Message, Thinking, TurnAccumulator};

    fn chunk_with_content(s: &str) -> String {
        format!(
            r#"{{"id":"1","choices":[{{"index":0,"delta":{{"content":"{s}"}},"finish_reason":null}}],"created":1,"model":"deepseek-v4-flash","object":"chat.completion.chunk"}}"#
        )
    }

    fn openai_stream(body: &str) -> SseStream {
        let stream = futures_util::stream::iter(vec![Ok(bytes::Bytes::from(body.to_owned()))]);
        SseStream {
            inner: Box::pin(stream),
            buf: Vec::new(),
            done: false,
            wire: Wire::OpenAi,
            transcoder: Transcoder::default(),
        }
    }

    #[test]
    fn parse_data_line() {
        let line = chunk_with_content("hi");
        match parse_sse_line(&format!("data: {line}")).unwrap() {
            SseLine::Chunk(c) => {
                assert_eq!(
                    c.choices[0].delta.as_ref().unwrap().content.as_deref(),
                    Some("hi")
                );
            }
            _ => panic!("expected Chunk"),
        }
    }

    #[test]
    fn parse_done_line() {
        assert!(matches!(
            parse_sse_line("data: [DONE]").unwrap(),
            SseLine::Done
        ));
        assert!(matches!(
            parse_sse_line("data:[DONE]").unwrap(),
            SseLine::Done
        ));
    }

    #[test]
    fn ignores_non_data_lines() {
        assert!(matches!(parse_sse_line("").unwrap(), SseLine::Ignored));
        assert!(matches!(
            parse_sse_line(": keep-alive").unwrap(),
            SseLine::Ignored
        ));
        assert!(matches!(
            parse_sse_line("event: ping").unwrap(),
            SseLine::Ignored
        ));
    }

    #[test]
    fn bad_json_is_error() {
        assert!(parse_sse_line("data: {broken").is_err());
    }

    #[test]
    fn take_line_handles_crlf_and_split_buffers() {
        let mut buf: Vec<u8> = b"data: {\"a\":1}\r\ndata: [DONE]\nresidual".to_vec();
        let l1 = take_line(&mut buf).unwrap();
        assert_eq!(l1, "data: {\"a\":1}");
        let l2 = take_line(&mut buf).unwrap();
        assert_eq!(l2, "data: [DONE]");
        assert!(take_line(&mut buf).is_none()); // no newline yet, stays buffered
        assert_eq!(buf, b"residual");
    }

    #[tokio::test]
    async fn stream_consumes_multiple_chunks_across_buffer_boundaries() {
        // Simulate two network packets: the first one cuts the second line's JSON
        let body = format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n",
            chunk_with_content("a"),
            chunk_with_content("b")
        );
        let bytes = body.as_bytes();
        let split_at = body.find("data: {").unwrap() + 12; // cut inside the 2nd line's JSON
        let (p1, p2) = bytes.split_at(split_at);
        let stream = futures_util::stream::iter(vec![
            Ok(bytes::Bytes::from(p1.to_vec())),
            Ok(bytes::Bytes::from(p2.to_vec())),
        ]);
        let mut sse = SseStream {
            inner: Box::pin(stream),
            buf: Vec::new(),
            done: false,
            wire: Wire::OpenAi,
            transcoder: Transcoder::default(),
        };

        let c1 = sse.next_chunk().await.unwrap().unwrap();
        assert_eq!(
            c1.choices[0].delta.as_ref().unwrap().content.as_deref(),
            Some("a")
        );
        let c2 = sse.next_chunk().await.unwrap().unwrap();
        assert_eq!(
            c2.choices[0].delta.as_ref().unwrap().content.as_deref(),
            Some("b")
        );
        assert!(sse.next_chunk().await.unwrap().is_none());
        assert!(sse.next_chunk().await.unwrap().is_none());
    }

    #[test]
    fn request_body_shape() {
        let req = ChatRequest {
            model: "deepseek-v4-flash".into(),
            max_tokens: 384_000,
            messages: vec![Message::system("sys"), Message::user("hi")],
            tools: None,
            tool_choice: None,
            stream: true,
            thinking: Some(Thinking::enabled()),
            reasoning_effort: Some("max".into()),
        };
        let v: serde_json::Value = serde_json::to_value(&req).unwrap();
        assert_eq!(v["model"], "deepseek-v4-flash");
        assert_eq!(v["max_tokens"], 384_000);
        assert_eq!(v["stream"], true);
        assert_eq!(v["thinking"]["type"], "enabled");
        assert_eq!(v["reasoning_effort"], "max");
        assert_eq!(v["messages"].as_array().unwrap().len(), 2);
    }

    // -----------------------------------------------------------------------
    // The Anthropic wire.
    // -----------------------------------------------------------------------

    fn event(name: &str, payload: serde_json::Value) -> String {
        format!("event: {name}\ndata: {payload}\n\n")
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
        let mut sse = openai_stream(&body);
        sse.wire = Wire::Anthropic;

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
        assert_eq!(msg.reasoning_content.as_deref(), Some("let me check."));
        assert_eq!(
            msg.thinking,
            Some(vec![crate::types::ThinkingBlock {
                thinking: "let me check.".into(),
                signature: "sig-1".into(),
            }])
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
        let mut sse = openai_stream(&body);
        sse.wire = Wire::Anthropic;
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
        let mut sse = openai_stream(&body);
        sse.wire = Wire::Anthropic;
        let err = sse.next_chunk().await.unwrap_err().to_string();
        assert!(err.contains("overloaded_error"), "{err}");
    }

    #[test]
    fn a_request_shape_that_does_not_match_the_wire_is_refused() {
        // No network is touched: the body is built before the request is
        // sent, and the mismatch is found there.
        let client =
            Client::new("k".into(), "http://127.0.0.1:1/never".into(), Wire::OpenAi).unwrap();
        let wrong = WireRequest::Anthropic(crate::types::AnthropicRequest {
            model: "MiniMax-M3".into(),
            max_tokens: 131_072,
            system: None,
            messages: vec![],
            tools: None,
            tool_choice: None,
            stream: true,
            thinking: None,
        });
        let err = client.body(&wrong).unwrap_err().to_string();
        assert!(err.contains("does not match"), "{err}");
        // The provider's own preset builds the shape its wire speaks, and a
        // client bound to that wire takes it.
        let right = crate::agent::request::build_request(
            &MINIMAX,
            &crate::session::SessionMeta {
                provider: Some("minimax".into()),
                model: "MiniMax-M3".into(),
                reasoning_effort: None,
                instructions: None,
            },
            &[],
        );
        let mini = Client::new("k".into(), MINIMAX.url.into(), Wire::Anthropic).unwrap();
        assert!(mini.body(&right).is_ok());
        assert!(client.body(&right).is_err());
    }
}
