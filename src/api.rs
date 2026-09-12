use anthropic::{BlockDelta, BlockKind, Event as AnthropicEvent};
use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use std::collections::HashMap;
use std::pin::Pin;

use crate::provider::{Provider, Wire};
use crate::types::{ChatChunk, Delta, DeltaFunctionCall, DeltaToolCall, Usage, WireRequest};

// ============================================================================
// The two wires, one conversation.
//
// The OpenAI wire is parsed here: `parse_sse_line` and `take_line` are pure
// functions and unit-testable, and `SseStream` yields one chunk at a time.
// Streaming contract: delta.reasoning_content precedes delta.content;
// `data: [DONE]` ends the stream; usage rides on the last content block (there
// is no standalone usage block).
//
// The Anthropic wire is the `anthropic` crate's: it speaks the standard
// protocol and hands over typed events, and `AnthropicStream` folds those into
// the very same deltas the accumulator reads. Nothing downstream learns that a
// second protocol exists — the pieces the OpenAI wire has no field for ride
// fields added for them (the signature that closes a thinking block, the usage
// split across message_start and message_delta).
// ============================================================================

type ByteStream = Pin<Box<dyn futures_util::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>;

/// The client for one provider: its endpoint, its key and its wire, paired so
/// that one provider's key cannot be sent to another provider's endpoint.
pub struct Client {
    backend: Backend,
}

enum Backend {
    /// The OpenAI chat-completions wire, spoken from here.
    OpenAi {
        http: reqwest::Client,
        api_key: String,
        url: String,
    },
    /// The Anthropic wire, spoken by the SDK.
    Anthropic(anthropic::Client),
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
            Wire::OpenAi => {
                let http = reqwest::Client::builder()
                    .connect_timeout(std::time::Duration::from_secs(30))
                    .build()
                    .context("failed to build HTTP client")?;
                Backend::OpenAi { http, api_key, url }
            }
            // The endpoint is the provider's own URL, path and all: an
            // Anthropic-compatible gateway puts the messages where it likes.
            Wire::Anthropic => Backend::Anthropic(anthropic::Client::new(
                anthropic::Profile::endpoint(url).with_bearer_token(api_key),
            )?),
        };
        Ok(Self { backend })
    }

    /// The OpenAI-wire request bytes. A shape handed to the other wire's
    /// endpoint is refused here rather than serialized across, which is the one
    /// misuse the WireRequest enum cannot prevent.
    fn body(&self, req: &WireRequest) -> Result<Vec<u8>> {
        match (&self.backend, req) {
            (Backend::OpenAi { .. }, WireRequest::OpenAi(r)) => Ok(serde_json::to_vec(r)?),
            _ => bail!("request shape does not match the provider's wire protocol"),
        }
    }

    /// Send one sub-request and hand back its stream.
    pub async fn stream_chat(&self, req: &WireRequest) -> Result<ChunkStream> {
        match (&self.backend, req) {
            (Backend::OpenAi { http, api_key, url }, WireRequest::OpenAi(_)) => {
                let resp = http
                    .post(url)
                    .bearer_auth(api_key)
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
                Ok(ChunkStream::OpenAi(SseStream {
                    inner: Box::pin(resp.bytes_stream()),
                    buf: Vec::new(),
                    done: false,
                }))
            }
            (Backend::Anthropic(client), WireRequest::Anthropic(r)) => Ok(ChunkStream::Anthropic(
                AnthropicStream::new(client.stream(r).await?),
            )),
            _ => bail!("request shape does not match the provider's wire protocol"),
        }
    }
}

/// One sub-request in flight, whichever wire carries it.
pub enum ChunkStream {
    /// The OpenAI wire's own SSE parser.
    OpenAi(SseStream),
    /// The Anthropic wire, adapted from the SDK's typed events.
    Anthropic(AnthropicStream),
}

impl std::fmt::Debug for ChunkStream {
    /// Which wire the stream is on, and not the bytes in it: a half-read event
    /// printed to a log is noise, while the wire is what a reader is looking
    /// for when something arrived in the wrong shape.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChunkStream::OpenAi(_) => f.write_str("ChunkStream::OpenAi(…)"),
            ChunkStream::Anthropic(_) => f.write_str("ChunkStream::Anthropic(…)"),
        }
    }
}

impl ChunkStream {
    /// None means the stream is over — the wire's end was reached or the
    /// connection closed.
    pub async fn next_chunk(&mut self) -> Result<Option<ChatChunk>> {
        match self {
            ChunkStream::OpenAi(sse) => sse.next_chunk().await,
            ChunkStream::Anthropic(stream) => stream.next_chunk().await,
        }
    }
}

/// The OpenAI wire's event stream.
pub struct SseStream {
    inner: ByteStream,
    buf: Vec<u8>,
    done: bool,
}

impl SseStream {
    /// None means the stream is over (the wire's end — `[DONE]` on the OpenAI
    /// shape, message_stop or connection close on the Anthropic one — was
    /// reached or the connection closed).
    pub async fn next_chunk(&mut self) -> Result<Option<ChatChunk>> {
        loop {
            if let Some(line) = take_line(&mut self.buf) {
                match parse_sse_line(&line)? {
                    SseLine::Chunk(c) => return Ok(Some(c)),
                    SseLine::Done => {
                        self.done = true;
                        return Ok(None);
                    }
                    SseLine::Ignored => continue,
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

    /// A body of SSE events as the SDK reads it, adapted by the stream the
    /// interpreter folds: the same bytes a mock endpoint would answer with.
    fn anthropic_stream(body: &str) -> ChunkStream {
        let bytes = bytes::Bytes::from(body.to_owned());
        let events = futures_util::stream::iter(vec![Ok(bytes)]);
        ChunkStream::Anthropic(AnthropicStream::new(anthropic::EventStream::new(Box::pin(
            events,
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
        let client =
            Client::new("k".into(), "http://127.0.0.1:1/never".into(), Wire::OpenAi).unwrap();
        let mini = Client::new("k".into(), MINIMAX.url.into(), Wire::Anthropic).unwrap();
        let meta = crate::session::SessionMeta {
            provider: Some("minimax".into()),
            model: "MiniMax-M3".into(),
            reasoning_effort: None,
            instructions: None,
        };
        let anthropic_request = crate::agent::request::build_request(&MINIMAX, &meta, &[]);
        let err = client.body(&anthropic_request).unwrap_err().to_string();
        assert!(err.contains("does not match"), "{err}");

        let mut deepseek_meta = meta.clone();
        deepseek_meta.provider = Some("deepseek".into());
        deepseek_meta.model = "deepseek-flash".into();
        let openai_request =
            crate::agent::request::build_request(&crate::provider::DEEPSEEK, &deepseek_meta, &[]);
        let err = mini
            .stream_chat(&openai_request)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not match"), "{err}");
    }
}
