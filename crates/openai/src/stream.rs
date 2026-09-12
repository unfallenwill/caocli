//! The chunk stream: Server-Sent Events in, [`ChatCompletionChunk`]s out.
//!
//! The framing is the SSE specification's, line for line: a line ends at `\n`,
//! `\r\n` or a lone `\r`; a line that begins with `:` is a comment and is
//! dropped; other lines are a field name and a value, one leading space after
//! the colon being the separator's own; and **an event is dispatched by the
//! blank line that ends it**, with its `data:` lines joined by newlines.
//!
//! The Chat Completions wire does not use `event:` lines — every event rides
//! on a `data:` payload. The reference client reads them with a `SSEDecoder`
//! that yields one event at a time; this crate does the same.
//!
//! Two lines of business are the wire's own:
//!
//! - **`data: [DONE]` ends the stream.** The literal `[DONE]` is the only
//!   signal the endpoint sends to say the answer is over, and what arrives
//!   after it is nothing. A stream that reports its own failure mid-answer
//!   does so with a `data:` payload whose body is `{"error": {...}}`; that
//!   ends the stream as an [`Error::Stream`], and what arrived before the
//!   report is not an answer.
//! - **A payload that is not the spec fails the stream.** Skipping it would
//!   drop whatever it carried from an answer the caller believes is whole.
//!
//! The stream is a sequence, not a session: [`ChunkStream::next_chunk`]
//! hands over one chunk at a time, and the caller decides what a completed
//! answer is. That is deliberate — the shape an answer accumulates into is
//! the caller's business, and two callers may want different ones.

use std::pin::Pin;

use bytes::Bytes;
use futures_util::StreamExt;

use crate::error::{Error, StreamError};
use crate::types::ChatCompletionChunk;

/// The bytes a stream reads from: whatever transport the caller has, as long as
/// it is `Send` and says nothing until the answer arrives.
pub type ByteStream = Pin<Box<dyn futures_util::Stream<Item = reqwest::Result<Bytes>> + Send>>;

/// The chunks of one answer, arriving as they are written.
pub struct ChunkStream {
    inner: ByteStream,
    /// Bytes that arrived without a complete event in them yet.
    buf: Vec<u8>,
    /// The wire said the answer was over (`[DONE]`, the connection closed, or
    /// a stream error ended it). Either way there is nothing more to read.
    done: bool,
}

impl std::fmt::Debug for ChunkStream {
    /// What the stream is carrying, not the bytes: a half-read event printed to
    /// a log is noise, and the counts are what a reader wants.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChunkStream")
            .field("buffered_bytes", &self.buf.len())
            .field("done", &self.done)
            .finish_non_exhaustive()
    }
}

impl ChunkStream {
    /// The stream over a byte source: the client builds one from a response,
    /// and a caller with a transport of its own — a test, a replay of a
    /// recorded answer — builds one from that.
    pub fn new(inner: ByteStream) -> Self {
        Self {
            inner,
            buf: Vec::new(),
            done: false,
        }
    }

    /// The next chunk, or `None` when the answer is over.
    ///
    /// The wire's `[DONE]` is consumed and then the stream is done: the caller
    /// sees the end of the answer as `None`, not as a chunk that needed
    /// handling. A stream that reported its own failure
    /// (`data: {"error": {...}}`) ends the call with [`Error::Stream`] — an
    /// answer that stopped mid-thought must not be read as one that finished.
    /// A frame that carried no `data:` line at all is read past, exactly as
    /// the reference client's own loop skips empty frames: a heartbeat frame
    /// is a boundary, not a chunk.
    pub async fn next_chunk(&mut self) -> Result<Option<ChatCompletionChunk>, Error> {
        loop {
            if let Some(lines) = take_frame(&mut self.buf) {
                let Some(payload) = payload_of(&lines) else {
                    // A frame with no data: a heartbeat, a comment, an
                    // `event:`-only frame. Read past, the way the reference
                    // client's decoder does.
                    continue;
                };
                // `[DONE]` is the wire's way of saying the answer is over.
                // What the reference client does is break the loop; we close
                // the stream and return `None` so the call after this one sees
                // it too.
                if payload == "[DONE]" {
                    self.done = true;
                    return Ok(None);
                }
                let value: serde_json::Value =
                    serde_json::from_str(&payload).map_err(|source| Error::Decode {
                        what: "SSE chunk",
                        source,
                    })?;
                // A stream that reported its own failure is over, and what
                // arrived before the report is not an answer: the caller
                // learns it from the error rather than from a chunk it must
                // remember to look for.
                if let Some(error) = value.get("error") {
                    let parsed: StreamError =
                        serde_json::from_value(error.clone()).map_err(|source| Error::Decode {
                            what: "stream error payload",
                            source,
                        })?;
                    self.done = true;
                    return Err(Error::Stream(parsed));
                }
                let chunk: ChatCompletionChunk =
                    serde_json::from_value(value).map_err(|source| Error::Decode {
                        what: "SSE chunk",
                        source,
                    })?;
                return Ok(Some(chunk));
            }
            if self.done {
                return Ok(None);
            }
            match self.inner.next().await {
                Some(Ok(bytes)) => self.buf.extend_from_slice(&bytes),
                Some(Err(e)) => return Err(Error::Transport(e)),
                // The connection closed. Without `[DONE]` this is a truncated
                // answer, and the caller is the one that can tell — it knows
                // whether the chunks it saw said the answer was over.
                None => self.done = true,
            }
        }
    }
}

/// Take one complete event's lines out of the buffer, if one has arrived.
///
/// Nothing is taken until the **blank line** that ends the event is in the
/// buffer, which is the spec's rule and the reason a terminator split across two
/// reads cannot do damage: `…\r` alone is not a blank line, and waiting for the
/// next byte is what tells `\r\n` from `\r\r`.
fn take_frame(buf: &mut Vec<u8>) -> Option<Vec<String>> {
    let end = frame_end(buf)?;
    let frame: Vec<u8> = buf.drain(..end).collect();
    Some(split_lines(&frame))
}

/// Where the first blank line ends, one past its last terminator byte. `None`
/// while none of the three spellings — `\n\n`, `\r\n\r\n`, `\r\r` — has
/// arrived.
fn frame_end(buf: &[u8]) -> Option<usize> {
    for (i, window) in buf.windows(2).enumerate() {
        if window == b"\n\n" || window == b"\r\r" {
            return Some(i + 2);
        }
        if window == b"\r\n" && buf[i + 2..].starts_with(b"\r\n") {
            return Some(i + 4);
        }
    }
    None
}

/// Cut one complete event into lines. All three terminators end a line, and the
/// blank line that ended the event arrives as its empty last line.
///
/// The chunk is known to be complete — [`frame_end`] found its blank line — so
/// a `\r` at the very end is a terminator here and not a prefix of something.
fn split_lines(frame: &[u8]) -> Vec<String> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < frame.len() {
        match frame[i] {
            b'\n' => {
                lines.push(lossy(&frame[start..i]));
                i += 1;
                start = i;
            }
            b'\r' => {
                lines.push(lossy(&frame[start..i]));
                let next_is_lf = frame.get(i + 1) == Some(&b'\n');
                i += if next_is_lf { 2 } else { 1 };
                start = i;
            }
            _ => i += 1,
        }
    }
    if start < frame.len() {
        lines.push(lossy(&frame[start..]));
    }
    lines
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The payload of one complete event: the `data:` lines joined by newlines,
/// as the spec joins them. None when the event carried no data at all.
fn payload_of(lines: &[String]) -> Option<String> {
    let mut data: Vec<&str> = Vec::new();
    for line in lines {
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let Some((field, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.strip_prefix(' ').unwrap_or(value);
        // Only `data:` fields ride on this wire — `event:`, `id:`, `retry:`
        // are SSE-spec fields and the Chat Completions wire does not use them.
        if field == "data" {
            data.push(value);
        }
    }
    if data.is_empty() {
        return None;
    }
    Some(data.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn stream_of(body: &str) -> ChunkStream {
        let bytes = Bytes::from(body.to_owned());
        ChunkStream::new(Box::pin(futures_util::stream::iter(vec![Ok(bytes)])))
    }

    /// A chunk as the wire writes it: the data line and the blank line that
    /// closes it.
    fn chunk(payload: serde_json::Value) -> String {
        format!("data: {payload}\n\n")
    }

    #[tokio::test]
    async fn chunks_arrive_one_at_a_time_and_done_ends_the_stream() {
        let body = [
            chunk(json!({
                "id": "cmpl-1",
                "choices": [{"delta": {"role": "assistant", "content": ""}, "index": 0, "finish_reason": null}],
                "created": 1,
                "model": "gpt-4o",
                "object": "chat.completion.chunk",
            })),
            chunk(json!({
                "id": "cmpl-1",
                "choices": [{"delta": {"content": "hi"}, "index": 0, "finish_reason": null}],
                "created": 1,
                "model": "gpt-4o",
                "object": "chat.completion.chunk",
            })),
            chunk(json!({
                "id": "cmpl-1",
                "choices": [{"delta": {}, "index": 0, "finish_reason": "stop"}],
                "created": 1,
                "model": "gpt-4o",
                "object": "chat.completion.chunk",
            })),
            // [DONE] ends the stream. It is consumed and not delivered.
            "data: [DONE]\n\n".to_string(),
        ]
        .concat();
        let mut stream = stream_of(&body);
        let mut seen = Vec::new();
        while let Some(chunk) = stream.next_chunk().await.unwrap() {
            seen.push(chunk);
        }
        assert_eq!(seen.len(), 3, "three chunks, [DONE] is consumed: {seen:?}");
        assert_eq!(seen[0].choices[0].delta.content.as_deref(), Some(""));
        assert_eq!(seen[1].choices[0].delta.content.as_deref(), Some("hi"));
        assert_eq!(
            seen[2].choices[0].finish_reason.as_ref(),
            Some(&crate::types::FinishReason::Stop)
        );
        // And then it is over.
        assert_eq!(stream.next_chunk().await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_heartbeat_frame_is_read_past_and_the_stream_reads_on() {
        // A frame with no data: a `: heartbeat` line, an empty frame, an
        // event-only frame. The reference client's decoder skips them, and so
        // does this one.
        let body = [
            ": heartbeat\n\n".to_string(),
            "\n".to_string(),
            "event: ping\n\n".to_string(),
            chunk(json!({
                "id": "cmpl-1",
                "choices": [{"delta": {"content": "ok"}, "index": 0, "finish_reason": null}],
                "created": 1,
                "model": "gpt-4o",
                "object": "chat.completion.chunk",
            })),
        ]
        .concat();
        let mut stream = stream_of(&body);
        let chunk = stream.next_chunk().await.unwrap().unwrap();
        assert_eq!(chunk.choices[0].delta.content.as_deref(), Some("ok"));
        assert_eq!(stream.next_chunk().await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_stream_that_reports_its_own_failure_fails_the_call() {
        let body = chunk(json!({
            "error": {
                "message": "Server overloaded.",
                "type": "server_error",
                "code": "rate_limit_exceeded",
            }
        }));
        let mut stream = stream_of(&body);
        let err = stream.next_chunk().await.unwrap_err();
        assert!(err.is_transient(), "the endpoint failed mid-answer: {err}");
        assert!(err.to_string().contains("rate_limit_exceeded"), "{err}");
    }

    #[tokio::test]
    async fn a_payload_that_is_not_the_spec_fails_rather_than_being_skipped() {
        // Skipping it would drop whatever it carried from an answer the
        // caller believes is whole.
        let mut stream = stream_of("data: {not json}\n\n");
        let err = stream.next_chunk().await.unwrap_err();
        assert!(!err.is_transient());
        assert!(err.to_string().contains("SSE chunk"), "{err}");
    }

    #[tokio::test]
    async fn a_connection_that_closes_ends_the_stream() {
        // No [DONE]: the caller is the one that can tell a truncated answer
        // from a finished one, and it does that from the chunks it saw.
        let mut stream = stream_of("data: [DONE]\n\n");
        assert_eq!(stream.next_chunk().await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_line_split_across_two_reads_is_one_chunk() {
        let (head, tail) = (
            "data: {\"id\":\"cmpl-1\",\"choices\":[{\"delta\":{\"content\":\"h",
            "i\"},\"index\":0,\"finish_reason\":null}],\"created\":1,\"model\":\"gpt-4o\",\"object\":\"chat.completion.chunk\"}\n\n",
        );
        let chunks = vec![
            Ok(Bytes::from(head.to_owned())),
            Ok(Bytes::from(tail.to_owned())),
        ];
        let mut stream = ChunkStream::new(Box::pin(futures_util::stream::iter(chunks)));
        let chunk = stream.next_chunk().await.unwrap().unwrap();
        assert_eq!(chunk.choices[0].delta.content.as_deref(), Some("hi"));
    }

    #[test]
    fn a_frame_is_taken_only_once_the_blank_line_that_ends_it_arrived() {
        // The three spellings of a blank line, and the same event written
        // with each of them.
        for blank in ["\n\n", "\r\n\r\n", "\r\r"] {
            let mut buf = format!("data: ping{blank}").into_bytes();
            assert_eq!(
                take_frame(&mut buf),
                Some(vec!["data: ping".to_string(), String::new()])
            );
            assert!(buf.is_empty(), "{blank:?} left {buf:?}");
        }
        // Half of one is not one: the bytes are held, not guessed at.
        for partial in ["data: ping\n", "data: ping\r", "data: ping\r\n"] {
            let mut buf = partial.as_bytes().to_vec();
            assert_eq!(take_frame(&mut buf), None, "{partial:?}");
            assert_eq!(buf, partial.as_bytes(), "{partial:?} consumed bytes");
        }
    }

    #[test]
    fn a_frame_leaves_the_next_chunk_in_the_buffer() {
        let mut buf = b"data: one\n\ndata: two\n".to_vec();
        assert_eq!(
            take_frame(&mut buf).map(|lines| lines.len()),
            Some(2),
            "the data line, the blank line that ends it, and no more"
        );
        assert_eq!(buf, b"data: two\n", "the second chunk is still arriving");
    }

    #[test]
    fn payload_of_joins_data_lines_and_skips_everything_else() {
        // Per the SSE spec, a frame's data is the newlines-joined concatenation
        // of its `data:` lines. A `data:` line with no value still counts.
        let lines = vec![
            "data: {\"a\":1}".into(),
            "event: message".into(),
            ": comment".into(),
            "id: 42".into(),
            "data: \"more\"".into(),
            "".into(),
        ];
        assert_eq!(payload_of(&lines), Some("{\"a\":1}\n\"more\"".to_string()));
    }

    #[test]
    fn payload_of_returns_none_when_there_is_no_data_line() {
        let lines = vec!["event: ping".into(), "".into()];
        assert!(payload_of(&lines).is_none());
    }
}
