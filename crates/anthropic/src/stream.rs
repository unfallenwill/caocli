//! The event stream: Server-Sent Events in, [`Event`]s out.
//!
//! The framing rules are the spec's and nothing more: a line is data when it
//! starts with `data:`, the event's *name* line carries nothing this crate needs
//! because the payload names its own type, and every other line — blank,
//! comment, `event:` — is skipped. A line that claims to be data and is not the
//! JSON the spec describes fails the stream: a payload nobody can read is not
//! something to skip past, since the answer being assembled would silently miss
//! whatever it carried.
//!
//! The stream is a sequence, not a session: [`EventStream::next_event`] hands
//! over one event at a time, and the caller decides what a completed answer is.
//! That is deliberate — the shape an answer accumulates into is the caller's
//! business, and two callers may want different ones.

use std::pin::Pin;

use bytes::Bytes;
use futures_util::StreamExt;

use crate::error::Error;
use crate::types::Event;

type ByteStream = Pin<Box<dyn futures_util::Stream<Item = reqwest::Result<Bytes>> + Send>>;

/// The events of one answer, arriving as they are written.
pub struct EventStream {
    inner: ByteStream,
    /// Bytes that arrived without a complete line in them yet.
    buf: Vec<u8>,
    /// The wire said the answer was over (`message_stop`), or the connection
    /// closed. Either way there is nothing more to read.
    done: bool,
}

impl std::fmt::Debug for EventStream {
    /// What the stream is carrying, not the bytes: a half-read event printed to
    /// a log is noise, and the counts are what a reader wants.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventStream")
            .field("buffered_bytes", &self.buf.len())
            .field("done", &self.done)
            .finish_non_exhaustive()
    }
}

impl EventStream {
    /// The stream over a byte source. Not public in use: a caller gets one from
    /// the client.
    pub(crate) fn new(inner: ByteStream) -> Self {
        Self {
            inner,
            buf: Vec::new(),
            done: false,
        }
    }

    /// The next event, or `None` when the answer is over.
    ///
    /// `message_stop` is delivered and then the stream is done: the caller sees
    /// the end of the answer as an event, and the call after it returns `None`.
    /// A stream that reports its own failure does not come through here as an
    /// event — it fails the call, because an answer that stopped mid-thought
    /// must not be read as one that finished.
    pub async fn next_event(&mut self) -> Result<Option<Event>, Error> {
        loop {
            if let Some(line) = take_line(&mut self.buf) {
                let Some(data) = data_of(&line) else {
                    continue;
                };
                if data.trim().is_empty() {
                    continue;
                }
                let event = decode(data)?;
                // A stream that reported its own failure is over, and what
                // arrived before the report is not an answer: the caller learns
                // it from the error rather than from an event it must remember
                // to look for.
                if let Event::Error { error } = event {
                    self.done = true;
                    return Err(Error::Stream(error));
                }
                if matches!(event, Event::MessageStop) {
                    self.done = true;
                }
                return Ok(Some(event));
            }
            if self.done {
                return Ok(None);
            }
            match self.inner.next().await {
                Some(Ok(bytes)) => self.buf.extend_from_slice(&bytes),
                Some(Err(e)) => return Err(Error::Transport(e)),
                // The connection closed. Without a `message_stop` this is a
                // truncated answer, and the caller is the one that can tell —
                // it knows whether the events it saw said the answer was over.
                None => self.done = true,
            }
        }
    }
}

/// Read one line's worth of JSON, turning the two failures worth telling apart
/// into two errors.
fn decode(data: &str) -> Result<Event, Error> {
    serde_json::from_str(data).map_err(|source| Error::Decode {
        what: "SSE event",
        source,
    })
}

/// Pull one line out of the buffer, as soon as `\n` is in it. A trailing `\r`
/// (the separator `\r\n`, which the spec allows) is not part of the line, and
/// `None` means no complete line has arrived yet.
fn take_line(buf: &mut Vec<u8>) -> Option<String> {
    let pos = buf.iter().position(|&b| b == b'\n')?;
    let line: Vec<u8> = buf.drain(..=pos).collect();
    let mut s = String::from_utf8_lossy(&line[..line.len() - 1]).into_owned();
    if s.ends_with('\r') {
        s.pop();
    }
    Some(s)
}

/// The payload of a data line: `data: x` and `data:x` both yield `x`; any other
/// line (blank, comment, `event:`) yields `None`.
fn data_of(line: &str) -> Option<&str> {
    let data = line.strip_prefix("data:")?;
    Some(data.strip_prefix(' ').unwrap_or(data))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream_of(body: &str) -> EventStream {
        let bytes = Bytes::from(body.to_owned());
        EventStream::new(Box::pin(futures_util::stream::iter(vec![Ok(bytes)])))
    }

    /// An event as the wire writes it: the name line, the data line, and the
    /// blank line that closes it.
    fn event(name: &str, payload: serde_json::Value) -> String {
        format!("event: {name}\ndata: {payload}\n\n")
    }

    #[tokio::test]
    async fn events_arrive_one_at_a_time_and_the_answer_ends() {
        let body = [
            event(
                "message_start",
                serde_json::json!({"type":"message_start","message":{"id":"m1","usage":{"input_tokens":7}}}),
            ),
            event("ping", serde_json::json!({"type":"ping"})),
            event(
                "content_block_start",
                serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            ),
            event(
                "content_block_delta",
                serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}),
            ),
            event("content_block_stop", serde_json::json!({"type":"content_block_stop","index":0})),
            event(
                "message_delta",
                serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),
            ),
            event("message_stop", serde_json::json!({"type":"message_stop"})),
        ]
        .concat();
        let mut stream = stream_of(&body);
        let mut seen = Vec::new();
        while let Some(event) = stream.next_event().await.unwrap() {
            seen.push(event);
        }
        assert_eq!(seen.len(), 7, "every event is delivered once: {seen:?}");
        assert!(matches!(seen[0], Event::MessageStart { .. }));
        assert_eq!(seen[1], Event::Ping);
        assert!(matches!(seen[2], Event::ContentBlockStart { index: 0, .. }));
        assert!(matches!(seen[3], Event::ContentBlockDelta { index: 0, .. }));
        assert_eq!(seen[4], Event::ContentBlockStop { index: 0 });
        assert!(matches!(seen[5], Event::MessageDelta { .. }));
        assert_eq!(seen[6], Event::MessageStop);
        assert_eq!(
            stream.next_event().await.unwrap(),
            None,
            "and then it is over"
        );
    }

    #[tokio::test]
    async fn the_name_line_and_the_blank_separator_are_not_part_of_the_payload() {
        // The payload names its own type, so the event name is duplicated data;
        // a stream that wrote it differently still reads.
        let body = "event: misplaced\ndata:{\"type\":\"ping\"}\n\n";
        let mut stream = stream_of(body);
        assert_eq!(stream.next_event().await.unwrap(), Some(Event::Ping));
        assert_eq!(stream.next_event().await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_carriage_return_does_not_end_up_in_the_payload() {
        let body = "data: {\"type\":\"ping\"}\r\n\r\n";
        let mut stream = stream_of(body);
        assert_eq!(stream.next_event().await.unwrap(), Some(Event::Ping));
    }

    #[tokio::test]
    async fn a_stream_that_reports_its_own_failure_fails_the_call() {
        let body = event(
            "error",
            serde_json::json!({"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}),
        );
        let mut stream = stream_of(&body);
        let err = stream.next_event().await.unwrap_err();
        assert!(err.is_transient(), "the endpoint asked for another moment");
        assert!(err.to_string().contains("overloaded_error"), "{err}");
    }

    #[tokio::test]
    async fn a_payload_that_is_not_the_spec_fails_rather_than_being_skipped() {
        // Skipping it would drop whatever it carried from an answer the caller
        // believes is whole.
        let mut stream = stream_of("data: {not json}\n\n");
        let err = stream.next_event().await.unwrap_err();
        assert!(!err.is_transient());
        assert!(err.to_string().contains("SSE event"), "{err}");
    }

    #[tokio::test]
    async fn a_connection_that_closes_ends_the_stream() {
        // No message_stop: the caller is the one that can tell a truncated
        // answer from a finished one, and it does that from the events it saw.
        let mut stream = stream_of("data: {\"type\":\"ping\"}\n\n");
        assert_eq!(stream.next_event().await.unwrap(), Some(Event::Ping));
        assert_eq!(stream.next_event().await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_line_split_across_two_reads_is_one_event() {
        let (head, tail) = (
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"t",
            "ext\":\"hi\"}}\n\n",
        );
        let chunks = vec![
            Ok(Bytes::from(head.to_owned())),
            Ok(Bytes::from(tail.to_owned())),
        ];
        let mut stream = EventStream::new(Box::pin(futures_util::stream::iter(chunks)));
        assert_eq!(
            stream.next_event().await.unwrap(),
            Some(Event::ContentBlockDelta {
                index: 0,
                delta: crate::types::BlockDelta::TextDelta { text: "hi".into() },
            })
        );
    }

    #[tokio::test]
    async fn an_unknown_event_is_carried_and_the_stream_reads_on() {
        let body = [
            event("future", serde_json::json!({"type":"future_event","x":1})),
            event("ping", serde_json::json!({"type":"ping"})),
        ]
        .concat();
        let mut stream = stream_of(&body);
        assert_eq!(stream.next_event().await.unwrap(), Some(Event::Unknown));
        assert_eq!(stream.next_event().await.unwrap(), Some(Event::Ping));
    }

    #[test]
    fn a_line_only_exists_once_its_newline_has_arrived() {
        const PARTIAL: &str = "data: {\"type\":\"ping\"}";
        let mut buf = PARTIAL.as_bytes().to_vec();
        assert_eq!(take_line(&mut buf), None);
        assert_eq!(buf.len(), PARTIAL.len(), "nothing was consumed");
        buf.push(b'\n');
        assert_eq!(take_line(&mut buf).as_deref(), Some(PARTIAL));
        assert!(buf.is_empty());
    }

    #[test]
    fn only_a_data_line_has_a_payload() {
        assert_eq!(data_of("data: {\"a\":1}"), Some("{\"a\":1}"));
        assert_eq!(data_of("data:{\"a\":1}"), Some("{\"a\":1}"));
        assert_eq!(data_of("event: ping"), None);
        assert_eq!(data_of(": a comment"), None);
        assert_eq!(data_of(""), None);
    }
}
