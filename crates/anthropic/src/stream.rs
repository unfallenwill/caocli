//! The event stream: Server-Sent Events in, [`Event`]s out.
//!
//! The framing is the SSE specification's, line for line: a line ends at `\n`,
//! `\r\n` or a lone `\r`; a line that begins with `:` is a comment and is
//! dropped; other lines are a field name and a value, one leading space after
//! the colon being the separator's own; and **an event is dispatched by the
//! blank line that ends it**, with its `data:` lines joined by newlines. An
//! event whose payload is not the JSON the spec describes fails the stream: a
//! payload nobody can read is not something to skip past, since the answer being
//! assembled would silently miss whatever it carried.
//!
//! The event's *name* line is read even though this API puts the type inside the
//! payload — the two agree, and a stream that spells the type only in the name
//! is still a stream this reads. The spec's `id` and `retry` fields are dropped:
//! nothing here reconnects on its own, so there is nothing for them to steer.
//! A `ping` keep-alive and an event type this crate does not know are read past
//! rather than handed over — again what the reference client does, and the
//! reason neither appears in the sequence a caller iterates.
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

/// The bytes a stream reads from: whatever transport the caller has, as long as
/// it is `Send` and says nothing until the answer arrives.
pub type ByteStream = Pin<Box<dyn futures_util::Stream<Item = reqwest::Result<Bytes>> + Send>>;

/// The events of one answer, arriving as they are written.
pub struct EventStream {
    inner: ByteStream,
    /// Bytes that arrived without a complete event in them yet.
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

    /// The next event, or `None` when the answer is over.
    ///
    /// `message_stop` is delivered and then the stream is done: the caller sees
    /// the end of the answer as an event, and the call after it returns `None`.
    ///
    /// Two of the type's variants never come out of here. A keep-alive
    /// ([`Event::Ping`]) and a type the spec added later ([`Event::Unknown`]) are
    /// skipped, and a stream that reported its own failure ([`Event::Error`])
    /// fails the call instead — an answer that stopped mid-thought must not be
    /// read as one that finished. All three are parsed and none is delivered,
    /// which is the reference client's own behavior.
    pub async fn next_event(&mut self) -> Result<Option<Event>, Error> {
        loop {
            if let Some(lines) = take_frame(&mut self.buf) {
                // A frame the stream carried nothing in — a blank line, a
                // comment, a keep-alive — is a boundary and not an event.
                let Some(payload) = payload_of(&lines) else {
                    continue;
                };
                let event = decode(&payload)?;
                // The events a caller sees are the ones an answer is made of.
                // A keep-alive and a type the spec added later are read past,
                // which is what the reference client's own loop does: `ping`
                // continues, and a name it does not know falls through. The
                // variants stay in the type because a parser that failed on them
                // would fail on a stream that is merely ahead of this crate.
                if matches!(event, Event::Ping | Event::Unknown) {
                    continue;
                }
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
                i += if frame.get(i + 1) == Some(&b'\n') {
                    2
                } else {
                    1
                };
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

/// One event's payload, as the frame's fields built it.
struct Payload {
    /// The `data:` lines joined by newlines, as the spec joins them.
    data: String,
    /// The event's name, when the stream carried one.
    name: Option<String>,
}

/// The payload of one complete event, or `None` when it carried no data.
///
/// A line is a field name and a value, one leading space after the colon being
/// the separator's own; a line beginning with `:` is a comment and is dropped;
/// a line that is neither is nothing. The `id` and `retry` fields are read past:
/// nothing here reconnects on its own, so there is nothing for them to steer.
fn payload_of(lines: &[String]) -> Option<Payload> {
    let mut name = None;
    let mut data: Vec<&str> = Vec::new();
    for line in lines {
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let Some((field, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => name = Some(value.to_string()),
            "data" => data.push(value),
            _ => {}
        }
    }
    // A `data:` with nothing after it is a field with an empty value, which the
    // join would turn into a payload with nothing in it. That is not JSON
    // anyone could read, and it is not a failure either: the event was empty.
    if data.iter().all(|line| line.trim().is_empty()) {
        return None;
    }
    Some(Payload {
        data: data.join("\n"),
        name,
    })
}

/// Read one payload's JSON into an event.
///
/// The name the stream carried stands in for a `type` the payload left out: the
/// two are the same word written twice, and a stream that spells it once is
/// still a stream this reads. The tolerant path runs only for a payload the
/// strict one refused, so the common case parses once.
fn decode(payload: &Payload) -> Result<Event, Error> {
    let refused = match serde_json::from_str(&payload.data) {
        Ok(event) => return Ok(event),
        Err(source) => source,
    };
    if let Some(name) = &payload.name
        && let Ok(serde_json::Value::Object(mut event)) =
            serde_json::from_str::<serde_json::Value>(&payload.data)
        && !event.contains_key("type")
    {
        event.insert("type".into(), serde_json::Value::String(name.clone()));
        return serde_json::from_value(serde_json::Value::Object(event)).map_err(|source| {
            Error::Decode {
                what: "SSE event",
                source,
            }
        });
    }
    Err(Error::Decode {
        what: "SSE event",
        source: refused,
    })
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
            // A keep-alive rides in the middle of the answer and is not
            // delivered: what a caller iterates is what the answer is made of.
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
        assert_eq!(seen.len(), 6, "six events, and no keep-alive: {seen:?}");
        assert!(matches!(seen[0], Event::MessageStart { .. }));
        assert!(matches!(seen[1], Event::ContentBlockStart { index: 0, .. }));
        assert!(matches!(seen[2], Event::ContentBlockDelta { index: 0, .. }));
        assert_eq!(seen[3], Event::ContentBlockStop { index: 0 });
        assert!(matches!(seen[4], Event::MessageDelta { .. }));
        assert_eq!(seen[5], Event::MessageStop);
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
        let body = "event: misplaced\ndata:{\"type\":\"message_stop\"}\n\n";
        let mut stream = stream_of(body);
        assert_eq!(stream.next_event().await.unwrap(), Some(Event::MessageStop));
        assert_eq!(stream.next_event().await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_carriage_return_does_not_end_up_in_the_payload() {
        let body = "data: {\"type\":\"message_stop\"}\r\n\r\n";
        let mut stream = stream_of(body);
        assert_eq!(stream.next_event().await.unwrap(), Some(Event::MessageStop));
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
        let mut stream = stream_of("data: {\"type\":\"message_stop\"}\n\n");
        assert_eq!(stream.next_event().await.unwrap(), Some(Event::MessageStop));
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
    async fn an_unknown_event_is_read_past_and_the_stream_reads_on() {
        // The spec adds event types over time and tells clients to handle the
        // unknown ones gracefully; what a caller iterates is the events an answer
        // is made of, so an unknown one is skipped rather than delivered.
        let body = [
            event("future", serde_json::json!({"type":"future_event","x":1})),
            event("message_stop", serde_json::json!({"type":"message_stop"})),
        ]
        .concat();
        let mut stream = stream_of(&body);
        assert_eq!(stream.next_event().await.unwrap(), Some(Event::MessageStop));
        assert_eq!(stream.next_event().await.unwrap(), None);
    }

    #[test]
    fn a_frame_is_taken_only_once_the_blank_line_that_ends_it_arrived() {
        // The three spellings of a blank line, and the same event written with
        // each of them.
        for blank in ["\n\n", "\r\n\r\n", "\r\r"] {
            let mut buf = format!("event: ping{blank}").into_bytes();
            assert_eq!(
                take_frame(&mut buf),
                Some(vec!["event: ping".to_string(), String::new()])
            );
            assert!(buf.is_empty(), "{blank:?} left {buf:?}");
        }
        // Half of one is not one: the bytes are held, not guessed at.
        for partial in ["event: ping\n", "event: ping\r", "event: ping\r\n"] {
            let mut buf = partial.as_bytes().to_vec();
            assert_eq!(take_frame(&mut buf), None, "{partial:?}");
            assert_eq!(buf, partial.as_bytes(), "{partial:?} consumed bytes");
        }
    }

    #[test]
    fn a_frame_leaves_the_next_event_in_the_buffer() {
        let mut buf = b"event: one\ndata: {}\n\nevent: two\n".to_vec();
        assert_eq!(
            take_frame(&mut buf).map(|lines| lines.len()),
            Some(3),
            "the event's fields, the blank line that ends it, and no more"
        );
        assert_eq!(buf, b"event: two\n", "the second event is still arriving");
    }

    #[test]
    fn every_terminator_ends_a_line() {
        // `\n`, `\r\n` and a lone `\r`, in one frame: three lines and the
        // empty one that ended it.
        assert_eq!(
            split_lines(b"a\nb\r\nc\r"),
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
        // The empty line that ends an event is one of the lines of its frame,
        // because it is the line that ends it.
        assert_eq!(
            split_lines(b"data: {}\n\n"),
            vec!["data: {}".to_string(), String::new()]
        );
        // A frame that does not end on a terminator keeps its last line.
        assert_eq!(split_lines(b"a\nb"), vec!["a".to_string(), "b".to_string()]);
        // A byte that is not valid UTF-8 is not a failure to read the lines
        // around it.
        assert_eq!(
            split_lines(b"a\xff\nb"),
            vec!["a\u{fffd}".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn the_payload_is_the_data_lines_wherever_the_fields_fall() {
        // The name may trail the data the spec wants it to describe, and a
        // field nobody knows is nothing rather than a failure.
        let lines: Vec<String> = [
            "data: {}",
            ": a comment",
            "retry: 100",
            "a line with no colon",
            "event: ping",
            "",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let payload = payload_of(&lines).expect("the frame carried data");
        assert_eq!(payload.data, "{}");
        assert_eq!(payload.name.as_deref(), Some("ping"));
    }

    #[test]
    fn the_data_lines_of_one_event_are_joined_by_newlines() {
        // The spec's rule, and the reason an event is dispatched by the blank
        // line rather than by its first data line: a payload may be spread over
        // as many lines as the server likes.
        let lines = [
            "data: {\"type\":\"content_block_delta\",",
            "data: \"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}",
            "",
        ];
        let payload = payload_of(&lines.map(str::to_string)).unwrap();
        assert_eq!(
            payload.data,
            "{\"type\":\"content_block_delta\",\n\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}"
        );
    }

    #[test]
    fn a_frame_with_nothing_in_it_is_not_an_event() {
        for lines in [
            vec![String::new()],
            vec![": keep-alive".into()],
            vec!["data:".into()],
        ] {
            assert!(payload_of(&lines).is_none(), "{lines:?}");
        }
    }

    /// A body of SSE bytes, read one event at a time.
    async fn events_of(body: &str) -> Vec<Event> {
        let mut stream = stream_of(body);
        let mut seen = Vec::new();
        while let Some(event) = stream.next_event().await.expect("the events read") {
            seen.push(event);
        }
        seen
    }

    #[tokio::test]
    async fn an_event_spread_over_several_data_lines_is_one_event() {
        let body = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\ndata: \"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n";
        assert_eq!(
            events_of(body).await,
            vec![Event::ContentBlockDelta {
                index: 0,
                delta: crate::types::BlockDelta::TextDelta { text: "hi".into() },
            }]
        );
    }

    #[tokio::test]
    async fn the_events_name_stands_in_for_a_type_the_payload_left_out() {
        // The name and the payload's type are the same word written twice. A
        // stream that writes it once still reads — and reads as the event the
        // name says, rather than as an unknown one.
        assert_eq!(
            events_of("event: message_stop\ndata: {}\n\n").await,
            vec![Event::MessageStop]
        );
        // A name this crate does not know is an unknown event, and unknown
        // events are read past.
        assert_eq!(
            events_of("event: invented_later\ndata: {}\n\n").await,
            vec![]
        );
        // And a payload that names its own type is believed over the name: the
        // field is the authority the spec points at. Here the name is one that
        // would be skipped and the payload is one that is not.
        assert_eq!(
            events_of("event: ping\ndata: {\"type\":\"message_stop\"}\n\n").await,
            vec![Event::MessageStop]
        );
    }

    #[tokio::test]
    async fn a_comment_and_a_lone_blank_line_are_not_events() {
        // Keep-alives: the server says nothing, so neither does this.
        let body = ": keep-alive\n\n: another\n\ndata: {\"type\":\"message_stop\"}\n\n";
        assert_eq!(events_of(body).await, vec![Event::MessageStop]);
    }

    #[tokio::test]
    async fn a_stream_terminated_by_carriage_returns_reads_the_same() {
        let body = "event: message_stop\r\ndata: {\"type\":\"message_stop\"}\r\n\r\n";
        assert_eq!(events_of(body).await, vec![Event::MessageStop]);
        // A lone `\r` as the terminator is the spec's third spelling.
        let body = "event: message_stop\rdata: {\"type\":\"message_stop\"}\r\r";
        assert_eq!(events_of(body).await, vec![Event::MessageStop]);
    }

    #[tokio::test]
    async fn a_terminator_split_across_two_reads_does_not_split_the_event() {
        // The `\r` ends one read and the `\n` opens the next: taken as two
        // terminators that would be a blank line, and the event's own name
        // would be an event with no payload in it.
        for (first, second) in [
            (
                "event: message_stop\r",
                "\ndata: {\"type\":\"message_stop\"}\r\n\r\n",
            ),
            (
                "event: message_stop\r",
                "\ndata: {\"type\":\"message_stop\"}\r\r",
            ),
        ] {
            let chunks = vec![
                Ok(Bytes::from(first.to_owned())),
                Ok(Bytes::from(second.to_owned())),
            ];
            let mut stream = EventStream::new(Box::pin(futures_util::stream::iter(chunks)));
            assert_eq!(
                stream.next_event().await.unwrap(),
                Some(Event::MessageStop),
                "{first:?} + {second:?}"
            );
            assert_eq!(stream.next_event().await.unwrap(), None);
        }
    }
}
