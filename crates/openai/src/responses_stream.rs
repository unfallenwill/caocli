//! The Responses event stream: Server-Sent Events in, [`ResponseStreamEvent`]s out.
//!
//! The framing is the SSE specification's, line for line: a line ends at `\n`,
//! `\r\n` or a lone `\r`; a line that begins with `:` is a comment and is
//! dropped; other lines are a field name and a value, one leading space after
//! the colon being the separator's own; and **an event is dispatched by the
//! blank line that ends it**, with its `data:` lines joined by newlines.
//!
//! Two lines of business are the wire's own:
//!
//! - **`data: [DONE]` ends the stream.** The literal `[DONE]` is the only
//!   signal the endpoint sends to say the stream is over. A stream that
//!   reports its own failure mid-answer does so with an `error`-typed event
//!   (`{"type":"error", ...}`); that ends the stream as an [`Error::Stream`],
//!   and what arrived before the report is not an answer.
//! - **A payload that is not the spec fails the stream.** Skipping it would
//!   drop whatever it carried from an answer the caller believes is whole.
//!
//! Events are tagged by `type` on the wire. A type this crate does not know
//! yet is read past and not delivered, the way the reference client's own
//! loop skips unknown event names; the variants that come out of the stream
//! are the ones an answer is made of.

use std::pin::Pin;

use bytes::Bytes;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};

use crate::error::{Error, StreamError};
use crate::responses_types::{ResponseError, ResponseOutputItem, ResponseStatus};

/// The bytes a stream reads from: whatever transport the caller has, as long as
/// it is `Send` and says nothing until the answer arrives.
pub type ByteStream = Pin<Box<dyn futures_util::Stream<Item = reqwest::Result<Bytes>> + Send>>;

/// One event of a streaming Responses answer.
///
/// The reference client tags events by `type` on the wire and uses the tag
/// to pick the variant. A type this crate does not know yet is read past,
/// the way the reference client's own loop skips unknown event names.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseStreamEvent {
    /// The response was created.
    #[serde(rename = "response.created")]
    Created {
        /// The response that was created.
        response: ResponseSummary,
        /// The sequence number for this event.
        sequence_number: u64,
    },
    /// The response is in progress.
    #[serde(rename = "response.in_progress")]
    InProgress {
        /// The response that is in progress.
        response: ResponseSummary,
        /// The sequence number for this event.
        sequence_number: u64,
    },
    /// The response is complete. The terminal event of a successful stream.
    #[serde(rename = "response.completed")]
    Completed {
        /// The completed response.
        response: ResponseSummary,
        /// The sequence number for this event.
        sequence_number: u64,
    },
    /// The response failed.
    #[serde(rename = "response.failed")]
    Failed {
        /// The response that failed.
        response: ResponseSummary,
        /// The sequence number for this event.
        sequence_number: u64,
    },
    /// The response finished as incomplete.
    #[serde(rename = "response.incomplete")]
    Incomplete {
        /// The response that was incomplete.
        response: ResponseSummary,
        /// The sequence number for this event.
        sequence_number: u64,
    },
    /// A new output item is being added.
    #[serde(rename = "response.output_item.added")]
    OutputItemAdded {
        /// The index of the output item that was added.
        output_index: u64,
        /// The output item that was added.
        item: ResponseOutputItem,
        /// The sequence number for this event.
        sequence_number: u64,
    },
    /// An output item is marked done.
    #[serde(rename = "response.output_item.done")]
    OutputItemDone {
        /// The index of the output item that was marked done.
        output_index: u64,
        /// The output item that was marked done.
        item: ResponseOutputItem,
        /// The sequence number for this event.
        sequence_number: u64,
    },
    /// A new content part is being added.
    #[serde(rename = "response.content_part.added")]
    ContentPartAdded {
        /// The id of the output item this content part belongs to.
        item_id: String,
        /// The index of the output item this content part belongs to.
        output_index: u64,
        /// The index of the content part within the item.
        content_index: u64,
        /// The content part that was added.
        part: StreamContentPart,
        /// The sequence number for this event.
        sequence_number: u64,
    },
    /// A content part is done.
    #[serde(rename = "response.content_part.done")]
    ContentPartDone {
        /// The id of the output item this content part belongs to.
        item_id: String,
        /// The index of the output item this content part belongs to.
        output_index: u64,
        /// The index of the content part within the item.
        content_index: u64,
        /// The content part that was marked done.
        part: StreamContentPart,
        /// The sequence number for this event.
        sequence_number: u64,
    },
    /// A text delta is added to an output text part.
    #[serde(rename = "response.output_text.delta")]
    OutputTextDelta {
        /// The id of the output item the delta was added to.
        item_id: String,
        /// The index of the output item.
        output_index: u64,
        /// The index of the content part the delta was added to.
        content_index: u64,
        /// The text delta that was added.
        delta: String,
        /// The log probabilities of the tokens in the delta, when the
        /// endpoint reports them.
        #[serde(default)]
        logprobs: Vec<Value>,
        /// The sequence number for this event.
        sequence_number: u64,
    },
    /// An output text part is finalized.
    #[serde(rename = "response.output_text.done")]
    OutputTextDone {
        /// The id of the output item.
        item_id: String,
        /// The index of the output item.
        output_index: u64,
        /// The index of the content part.
        content_index: u64,
        /// The full text of the content part.
        text: String,
        /// The log probabilities of the tokens, when the endpoint reports them.
        #[serde(default)]
        logprobs: Vec<Value>,
        /// The sequence number for this event.
        sequence_number: u64,
    },
    /// A function-call arguments delta is added.
    #[serde(rename = "response.function_call_arguments.delta")]
    FunctionCallArgumentsDelta {
        /// The id of the output item.
        item_id: String,
        /// The index of the output item.
        output_index: u64,
        /// The arguments delta that was added.
        delta: String,
        /// The sequence number for this event.
        sequence_number: u64,
    },
    /// Function-call arguments are finalized.
    #[serde(rename = "response.function_call_arguments.done")]
    FunctionCallArgumentsDone {
        /// The id of the output item.
        item_id: String,
        /// The index of the output item.
        output_index: u64,
        /// The full arguments string.
        arguments: String,
        /// The sequence number for this event.
        sequence_number: u64,
    },
    /// A reasoning text delta is added.
    #[serde(rename = "response.reasoning_text.delta")]
    ReasoningTextDelta {
        /// The id of the reasoning item.
        item_id: String,
        /// The index of the output item.
        output_index: u64,
        /// The index of the content part the delta was added to.
        content_index: u64,
        /// The text delta that was added.
        delta: String,
        /// The sequence number for this event.
        sequence_number: u64,
    },
    /// A reasoning text is finalized.
    #[serde(rename = "response.reasoning_text.done")]
    ReasoningTextDone {
        /// The id of the reasoning item.
        item_id: String,
        /// The index of the output item.
        output_index: u64,
        /// The index of the content part.
        content_index: u64,
        /// The full text of the reasoning content.
        text: String,
        /// The sequence number for this event.
        sequence_number: u64,
    },
    /// A reasoning summary text delta is added.
    #[serde(rename = "response.reasoning_summary_text.delta")]
    ReasoningSummaryTextDelta {
        /// The id of the reasoning item.
        item_id: String,
        /// The index of the output item.
        output_index: u64,
        /// The index of the summary part the delta was added to.
        summary_index: u64,
        /// The text delta that was added.
        delta: String,
        /// The sequence number for this event.
        sequence_number: u64,
    },
    /// A reasoning summary text is finalized.
    #[serde(rename = "response.reasoning_summary_text.done")]
    ReasoningSummaryTextDone {
        /// The id of the reasoning item.
        item_id: String,
        /// The index of the output item.
        output_index: u64,
        /// The index of the summary part.
        summary_index: u64,
        /// The full text of the reasoning summary.
        text: String,
        /// The sequence number for this event.
        sequence_number: u64,
    },
    /// A reasoning summary part is added.
    #[serde(rename = "response.reasoning_summary_part.added")]
    ReasoningSummaryPartAdded {
        /// The id of the reasoning item.
        item_id: String,
        /// The index of the output item.
        output_index: u64,
        /// The index of the summary part that was added.
        summary_index: u64,
        /// The summary part that was added.
        part: ReasoningSummaryPart,
        /// The sequence number for this event.
        sequence_number: u64,
    },
    /// A reasoning summary part is finalized.
    #[serde(rename = "response.reasoning_summary_part.done")]
    ReasoningSummaryPartDone {
        /// The id of the reasoning item.
        item_id: String,
        /// The index of the output item.
        output_index: u64,
        /// The index of the summary part.
        summary_index: u64,
        /// The summary part that was marked done.
        part: ReasoningSummaryPart,
        /// The sequence number for this event.
        sequence_number: u64,
    },
    /// A refusal text delta is added.
    #[serde(rename = "response.refusal.delta")]
    RefusalDelta {
        /// The id of the output item the delta was added to.
        item_id: String,
        /// The index of the output item.
        output_index: u64,
        /// The index of the content part the delta was added to.
        content_index: u64,
        /// The text delta that was added.
        delta: String,
        /// The sequence number for this event.
        sequence_number: u64,
    },
    /// A refusal text is finalized.
    #[serde(rename = "response.refusal.done")]
    RefusalDone {
        /// The id of the output item.
        item_id: String,
        /// The index of the output item.
        output_index: u64,
        /// The index of the content part.
        content_index: u64,
        /// The full text of the refusal.
        refusal: String,
        /// The sequence number for this event.
        sequence_number: u64,
    },
    /// An annotation is added to an output text part.
    #[serde(rename = "response.output_text.annotation.added")]
    OutputTextAnnotationAdded {
        /// The id of the output item the annotation was added to.
        item_id: String,
        /// The index of the output item.
        output_index: u64,
        /// The index of the content part the annotation was added to.
        content_index: u64,
        /// The index of the annotation within the content part.
        annotation_index: u64,
        /// The annotation that was added.
        annotation: Value,
        /// The sequence number for this event.
        sequence_number: u64,
    },
    /// The response has been queued for background processing.
    #[serde(rename = "response.queued")]
    Queued {
        /// The response that was queued.
        response: ResponseSummary,
        /// The sequence number for this event.
        sequence_number: u64,
    },
    /// A partial chunk of base64-encoded audio bytes.
    #[serde(rename = "response.audio.delta")]
    AudioDelta {
        /// The chunk of base64 audio bytes.
        delta: String,
        /// The sequence number for this event.
        sequence_number: u64,
    },
    /// The audio response is complete.
    #[serde(rename = "response.audio.done")]
    AudioDone {
        /// The sequence number for this event.
        sequence_number: u64,
    },
    /// A partial transcript of the audio response.
    #[serde(rename = "response.audio.transcript.delta")]
    AudioTranscriptDelta {
        /// The partial transcript text.
        delta: String,
        /// The sequence number for this event.
        sequence_number: u64,
    },
    /// The full audio transcript is complete.
    #[serde(rename = "response.audio.transcript.done")]
    AudioTranscriptDone {
        /// The sequence number for this event.
        sequence_number: u64,
    },
    /// An error occurred mid-stream.
    #[serde(rename = "error")]
    Error {
        /// The error code.
        #[serde(default)]
        code: Option<String>,
        /// The error message.
        message: String,
        /// The parameter the endpoint blamed, when it blamed one.
        #[serde(default)]
        param: Option<String>,
        /// The sequence number for this event.
        sequence_number: u64,
    },
}

/// The content part carried on a stream event. Tagged by `type` on the wire.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamContentPart {
    /// An output text part.
    #[serde(rename = "output_text")]
    OutputText {
        /// The text of the content part.
        text: String,
        /// The annotations on the text, when the model added some.
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

/// A summary part on a reasoning item: a single line of the model's
/// summarized reasoning.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ReasoningSummaryPart {
    /// The text of the summary part.
    pub text: String,
    /// The kind of summary, always `"summary_text"`.
    #[serde(rename = "type")]
    pub r#type: String,
}

/// A summary of the response carried on lifecycle events. Tolerant by
/// construction: it carries the fields a streaming event needs (id, status,
/// model, usage) without forcing every field of the full [`crate::Response`]
/// to be present mid-stream.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct ResponseSummary {
    /// The id of the response.
    pub id: String,
    /// The status of the response, when one has been set.
    #[serde(default)]
    pub status: Option<ResponseStatus>,
    /// The model that produced the response, when the endpoint has named one.
    #[serde(default)]
    pub model: Option<String>,
    /// The object type, always `"response"`.
    #[serde(default)]
    pub object: Option<String>,
    /// Unix timestamp the response was created at.
    #[serde(default)]
    pub created_at: Option<f64>,
    /// An error object, when the response failed.
    #[serde(default)]
    pub error: Option<ResponseError>,
    /// Token usage, when the endpoint reports one.
    #[serde(default)]
    pub usage: Option<ResponseUsageSummary>,
    /// The output items, when the event carries them (e.g. on completion).
    #[serde(default)]
    pub output: Vec<ResponseOutputItem>,
}

/// A summary of the usage block on a streaming event. Only the totals are
/// carried on lifecycle events; the breakdowns arrive on the final event.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct ResponseUsageSummary {
    /// Tokens the model spent on the input.
    #[serde(default)]
    pub input_tokens: u64,
    /// Tokens the model spent on the output.
    #[serde(default)]
    pub output_tokens: u64,
    /// `input_tokens + output_tokens`.
    #[serde(default)]
    pub total_tokens: u64,
}

/// A JSON value used by the stream events for fields like annotations and
/// logprobs that the crate hands over without interpreting.
pub type Value = serde_json::Value;

// ============================================================================
// Stream
// ============================================================================

/// The events of one answer, arriving as they are written.
pub struct ResponseEventStream {
    inner: ByteStream,
    /// Bytes that arrived without a complete event in them yet.
    buf: Vec<u8>,
    /// The wire said the stream was over (`[DONE]`, the connection closed, or
    /// a stream error ended it). Either way there is nothing more to read.
    done: bool,
}

impl std::fmt::Debug for ResponseEventStream {
    /// What the stream is carrying, not the bytes: a half-read event printed to
    /// a log is noise, and the counts are what a reader wants.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResponseEventStream")
            .field("buffered_bytes", &self.buf.len())
            .field("done", &self.done)
            .finish_non_exhaustive()
    }
}

impl ResponseEventStream {
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

    /// The next event, or `None` when the stream is over.
    ///
    /// The wire's `[DONE]` is consumed and then the stream is done: the caller
    /// sees the end of the answer as `None`, not as an event that needed
    /// handling. A stream that reported its own failure (`{"type":"error", ...}`)
    /// ends the call with [`Error::Stream`] — an answer that stopped
    /// mid-thought must not be read as one that finished. A frame that carried
    /// no `data:` line at all is read past, the way the reference client's
    /// own decoder does.
    pub async fn next_event(&mut self) -> Result<Option<ResponseStreamEvent>, Error> {
        loop {
            if let Some(lines) = take_frame(&mut self.buf) {
                let Some(payload) = payload_of(&lines) else {
                    // A frame with no data: a heartbeat, a comment, an
                    // `event:`-only frame. Read past, the way the reference
                    // client's decoder does.
                    continue;
                };
                // `[DONE]` is the wire's way of saying the stream is over.
                // The reference client breaks the loop on it; we close the
                // stream and return `None` so the call after this one sees
                // it too.
                if payload == "[DONE]" {
                    self.done = true;
                    return Ok(None);
                }
                let value: serde_json::Value =
                    serde_json::from_str(&payload).map_err(|source| Error::Decode {
                        what: "SSE event",
                        source,
                    })?;
                // A stream that reported its own failure is over, and what
                // arrived before the report is not an answer: the caller
                // learns it from the error rather than from an event it must
                // remember to look for.
                if value.get("type").and_then(Value::as_str) == Some("error") {
                    let parsed: StreamError =
                        serde_json::from_value(value).map_err(|source| Error::Decode {
                            what: "stream error payload",
                            source,
                        })?;
                    self.done = true;
                    return Err(Error::Stream(parsed));
                }
                // An event whose type is not in the enum: the reference
                // client's own decoder skips it. We read past and try the
                // next frame.
                let event: ResponseStreamEvent = match serde_json::from_value(value) {
                    Ok(event) => event,
                    Err(_) => continue,
                };
                return Ok(Some(event));
            }
            if self.done {
                return Ok(None);
            }
            match self.inner.next().await {
                Some(Ok(bytes)) => self.buf.extend_from_slice(&bytes),
                Some(Err(e)) => return Err(Error::Transport(e)),
                // The connection closed. Without `[DONE]` this is a truncated
                // stream, and the caller is the one that can tell — it knows
                // whether the events it saw said the answer was over.
                None => self.done = true,
            }
        }
    }
}

// ============================================================================
// SSE framing helpers (shared with chat completions stream)
// ============================================================================

/// Take one complete event's lines out of the buffer, if one has arrived.
fn take_frame(buf: &mut Vec<u8>) -> Option<Vec<String>> {
    let end = frame_end(buf)?;
    let frame: Vec<u8> = buf.drain(..end).collect();
    Some(split_lines(&frame))
}

/// Where the first blank line ends, one past its last terminator byte.
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

/// Cut one complete event into lines.
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

/// The payload of one complete event: the `data:` lines joined by newlines.
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

    fn stream_of(body: &str) -> ResponseEventStream {
        let bytes = Bytes::from(body.to_owned());
        ResponseEventStream::new(Box::pin(futures_util::stream::iter(vec![Ok(bytes)])))
    }

    fn event(payload: serde_json::Value) -> String {
        format!("data: {payload}\n\n")
    }

    #[tokio::test]
    async fn lifecycle_events_arrive_in_order_and_done_ends_the_stream() {
        let body = [
            event(json!({
                "type": "response.created",
                "response": {"id": "resp_1", "object": "response", "created_at": 1.0, "status": "in_progress"},
                "sequence_number": 0,
            })),
            event(json!({
                "type": "response.in_progress",
                "response": {"id": "resp_1", "object": "response", "created_at": 1.0, "status": "in_progress"},
                "sequence_number": 1,
            })),
            event(json!({
                "type": "response.completed",
                "response": {"id": "resp_1", "object": "response", "created_at": 1.0, "status": "completed"},
                "sequence_number": 2,
            })),
            "data: [DONE]\n\n".to_string(),
        ]
        .concat();
        let mut stream = stream_of(&body);
        let mut seen = Vec::new();
        while let Some(event) = stream.next_event().await.unwrap() {
            seen.push(event);
        }
        assert_eq!(seen.len(), 3, "three events, [DONE] is consumed: {seen:?}");
        assert!(matches!(seen[0], ResponseStreamEvent::Created { .. }));
        assert!(matches!(seen[1], ResponseStreamEvent::InProgress { .. }));
        assert!(matches!(seen[2], ResponseStreamEvent::Completed { .. }));
        assert_eq!(stream.next_event().await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_text_delta_and_done_arrive() {
        let body = [
            event(json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"id": "msg_1", "type": "message", "role": "assistant", "status": "in_progress", "content": []},
                "sequence_number": 0,
            })),
            event(json!({
                "type": "response.content_part.added",
                "item_id": "msg_1",
                "output_index": 0,
                "content_index": 0,
                "part": {"type": "output_text", "text": "", "annotations": []},
                "sequence_number": 1,
            })),
            event(json!({
                "type": "response.output_text.delta",
                "item_id": "msg_1",
                "output_index": 0,
                "content_index": 0,
                "delta": "hi",
                "logprobs": [],
                "sequence_number": 2,
            })),
            event(json!({
                "type": "response.output_text.done",
                "item_id": "msg_1",
                "output_index": 0,
                "content_index": 0,
                "text": "hi",
                "logprobs": [],
                "sequence_number": 3,
            })),
            event(json!({
                "type": "response.completed",
                "response": {"id": "resp_1", "object": "response", "created_at": 1.0, "status": "completed"},
                "sequence_number": 4,
            })),
            "data: [DONE]\n\n".to_string(),
        ]
        .concat();
        let mut stream = stream_of(&body);
        let mut seen = Vec::new();
        while let Some(event) = stream.next_event().await.unwrap() {
            seen.push(event);
        }
        assert_eq!(seen.len(), 5);
        if let ResponseStreamEvent::OutputTextDelta { delta, .. } = &seen[2] {
            assert_eq!(delta, "hi");
        } else {
            panic!("expected output text delta, got {:?}", seen[2]);
        }
        if let ResponseStreamEvent::OutputTextDone { text, .. } = &seen[3] {
            assert_eq!(text, "hi");
        } else {
            panic!("expected output text done, got {:?}", seen[3]);
        }
    }

    #[tokio::test]
    async fn a_function_call_arguments_event_pair_is_handled() {
        let body = [
            event(json!({
                "type": "response.function_call_arguments.delta",
                "item_id": "fc_1",
                "output_index": 0,
                "delta": "{\"city\":",
                "sequence_number": 0,
            })),
            event(json!({
                "type": "response.function_call_arguments.done",
                "item_id": "fc_1",
                "output_index": 0,
                "arguments": "{\"city\":\"sf\"}",
                "sequence_number": 1,
            })),
            "data: [DONE]\n\n".to_string(),
        ]
        .concat();
        let mut stream = stream_of(&body);
        let mut seen = Vec::new();
        while let Some(event) = stream.next_event().await.unwrap() {
            seen.push(event);
        }
        assert_eq!(seen.len(), 2);
        if let ResponseStreamEvent::FunctionCallArgumentsDelta { delta, .. } = &seen[0] {
            assert_eq!(delta, "{\"city\":");
        } else {
            panic!("expected delta, got {:?}", seen[0]);
        }
        if let ResponseStreamEvent::FunctionCallArgumentsDone { arguments, .. } = &seen[1] {
            assert_eq!(arguments, "{\"city\":\"sf\"}");
        } else {
            panic!("expected done, got {:?}", seen[1]);
        }
    }

    #[tokio::test]
    async fn a_stream_that_reports_its_own_failure_fails_the_call() {
        let body = event(json!({
            "type": "error",
            "code": "server_error",
            "message": "Server overloaded.",
            "sequence_number": 0,
        }));
        let mut stream = stream_of(&body);
        let err = stream.next_event().await.unwrap_err();
        assert!(err.is_transient(), "the endpoint failed mid-answer: {err}");
        assert!(err.to_string().contains("server_error"), "{err}");
    }

    #[tokio::test]
    async fn a_payload_that_is_not_the_spec_fails_rather_than_being_skipped() {
        let mut stream = stream_of("data: {not json}\n\n");
        let err = stream.next_event().await.unwrap_err();
        assert!(!err.is_transient());
        assert!(err.to_string().contains("SSE event"), "{err}");
    }

    #[tokio::test]
    async fn an_event_type_we_dont_know_is_read_past_and_the_stream_reads_on() {
        // The spec adds event types over time and tells clients to handle
        // the unknown ones gracefully. What a caller iterates is what the
        // answer is made of, so an unknown event type is skipped.
        let body = [
            event(json!({"type": "response.future_event", "x": 1})),
            event(json!({
                "type": "response.completed",
                "response": {"id": "resp_1", "status": "completed"},
                "sequence_number": 0,
            })),
            "data: [DONE]\n\n".to_string(),
        ]
        .concat();
        let mut stream = stream_of(&body);
        let event = stream.next_event().await.unwrap().unwrap();
        assert!(matches!(event, ResponseStreamEvent::Completed { .. }));
        assert_eq!(stream.next_event().await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_heartbeat_frame_is_read_past_and_the_stream_reads_on() {
        // A frame with no data: a `: heartbeat` line, an empty frame, an
        // event-only frame. The reference client's decoder skips them, and
        // so does this one.
        let body = [
            ": heartbeat\n\n".to_string(),
            "\n".to_string(),
            event(json!({
                "type": "response.completed",
                "response": {"id": "resp_1", "status": "completed"},
                "sequence_number": 0,
            })),
        ]
        .concat();
        let mut stream = stream_of(&body);
        let event = stream.next_event().await.unwrap().unwrap();
        assert!(matches!(event, ResponseStreamEvent::Completed { .. }));
    }

    #[tokio::test]
    async fn refusal_and_summary_events_are_handled() {
        let body = [
            event(json!({
                "type": "response.refusal.delta",
                "item_id": "msg_1", "output_index": 0, "content_index": 0,
                "delta": "I can't",
                "sequence_number": 0,
            })),
            event(json!({
                "type": "response.refusal.done",
                "item_id": "msg_1", "output_index": 0, "content_index": 0,
                "refusal": "I can't do that.",
                "sequence_number": 1,
            })),
            event(json!({
                "type": "response.reasoning_summary_text.delta",
                "item_id": "rs_1", "output_index": 0, "summary_index": 0,
                "delta": "thinking",
                "sequence_number": 2,
            })),
            event(json!({
                "type": "response.reasoning_summary_part.added",
                "item_id": "rs_1", "output_index": 0, "summary_index": 0,
                "part": {"type": "summary_text", "text": "thinking"},
                "sequence_number": 3,
            })),
            event(json!({
                "type": "response.queued",
                "response": {"id": "resp_1", "status": "queued"},
                "sequence_number": 4,
            })),
            event(json!({
                "type": "response.output_text.annotation.added",
                "item_id": "msg_1", "output_index": 0, "content_index": 0,
                "annotation_index": 0,
                "annotation": {"type": "url_citation", "url": "https://example.com"},
                "sequence_number": 5,
            })),
            "data: [DONE]\n\n".to_string(),
        ]
        .concat();
        let mut stream = stream_of(&body);
        let mut events = Vec::new();
        while let Some(e) = stream.next_event().await.unwrap() {
            events.push(e);
        }
        assert_eq!(events.len(), 6);
        assert!(matches!(
            events[0],
            ResponseStreamEvent::RefusalDelta { .. }
        ));
        assert!(matches!(events[1], ResponseStreamEvent::RefusalDone { .. }));
        assert!(matches!(
            events[2],
            ResponseStreamEvent::ReasoningSummaryTextDelta { .. }
        ));
        assert!(matches!(
            events[3],
            ResponseStreamEvent::ReasoningSummaryPartAdded { .. }
        ));
        assert!(matches!(events[4], ResponseStreamEvent::Queued { .. }));
        assert!(matches!(
            events[5],
            ResponseStreamEvent::OutputTextAnnotationAdded { .. }
        ));
    }

    #[tokio::test]
    async fn audio_chunk_events_are_handled() {
        let body = [
            event(json!({
                "type": "response.audio.delta",
                "delta": "AAAA",
                "sequence_number": 0,
            })),
            event(json!({
                "type": "response.audio.done",
                "sequence_number": 1,
            })),
            event(json!({
                "type": "response.audio.transcript.delta",
                "delta": "hello",
                "sequence_number": 2,
            })),
            event(json!({
                "type": "response.audio.transcript.done",
                "sequence_number": 3,
            })),
            "data: [DONE]\n\n".to_string(),
        ]
        .concat();
        let mut stream = stream_of(&body);
        let mut events = Vec::new();
        while let Some(e) = stream.next_event().await.unwrap() {
            events.push(e);
        }
        assert_eq!(events.len(), 4);
        if let ResponseStreamEvent::AudioDelta { delta, .. } = &events[0] {
            assert_eq!(delta, "AAAA");
        } else {
            panic!("expected AudioDelta");
        }
        assert!(matches!(events[1], ResponseStreamEvent::AudioDone { .. }));
        if let ResponseStreamEvent::AudioTranscriptDelta { delta, .. } = &events[2] {
            assert_eq!(delta, "hello");
        } else {
            panic!("expected AudioTranscriptDelta");
        }
        assert!(matches!(
            events[3],
            ResponseStreamEvent::AudioTranscriptDone { .. }
        ));
    }
}
