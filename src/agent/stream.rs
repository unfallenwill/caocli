//! One sub-request: the stream in, a complete assistant message out.
//!
//! The aggregation itself is `types::TurnAccumulator`'s (a pure fold over the
//! deltas, unit-tested without a network); what lives here is the part that has
//! to touch the stream and the front end: forward each delta to the watcher, and
//! time the tokens.

use std::time::{Duration, Instant};

use anyhow::Result;

use crate::types::{Message, TurnAccumulator, Usage, WireRequest};
use crate::ui::Ui;

use super::Agent;

/// What one sub-request produced. A named struct rather than a tuple: three
/// values that travel together and are easy to transpose, and a caller should
/// not have to remember which position the duration is in.
pub(super) struct Reply {
    /// The assistant message, unpersisted: the caller writes the log.
    pub(super) message: Message,
    /// Token usage as the backend reported it, if it reported any.
    pub(super) usage: Option<Usage>,
    /// The wall time from the first delta to the end of the stream: what a
    /// tokens-per-second figure divides the reported completion tokens by.
    /// Everything before the first delta — the handshake and, on a long
    /// prompt, the backend's whole prefill — is the wait for tokens, not the
    /// speed of them, and is left out.
    pub(super) stream_time: Duration,
    /// Why the backend stopped, in the word its wire uses — `stop`, `length`,
    /// `end_turn`, `max_tokens`, `tool_use`. `None` when the stream ended
    /// without saying, which a connection that closes early does.
    pub(super) finish_reason: Option<String>,
}

impl Reply {
    /// The notice owed to a front end when the answer was cut off, if it was.
    ///
    /// A truncated answer is persisted like any other — a half-written file is
    /// usually worth having — but nothing in the log says the rest never came.
    /// Without this, the only sign is a turn that stops making sense, and the
    /// reason the model stopped is a thing the backend told us and we dropped.
    pub(super) fn truncation(&self) -> Option<String> {
        let why = match self.finish_reason.as_deref()? {
            // The OpenAI wire's word for the ceiling, and the Anthropic
            // wire's.
            "length" | "max_tokens" => "the answer reached the provider's max_tokens ceiling",
            "model_context_window_exceeded" => {
                "the conversation no longer fits the provider's context window"
            }
            _ => return None,
        };
        Some(format!("{why}; what is in the log is incomplete"))
    }
}

impl Agent {
    /// Run one sub-request: consume the SSE stream (notifying the UI delta by
    /// delta) and aggregate a complete assistant message.
    /// Errors propagate upward; at that point the assistant message has not been
    /// persisted, so the session stays at a valid prefix.
    pub(super) async fn stream_reply(
        &self,
        request: &WireRequest,
        ui: &mut dyn Ui,
    ) -> Result<Reply> {
        let mut stream = self.api.stream_chat(request).await?;
        let mut started: Option<Instant> = None;
        let mut accumulator = TurnAccumulator::default();
        let mut usage: Option<Usage> = None;
        let mut finish_reason: Option<String> = None;
        while let Some(chunk) = stream.next_chunk().await? {
            for choice in chunk.choices {
                if let Some(reason) = &choice.finish_reason {
                    finish_reason = Some(reason.clone());
                }
                let Some(delta) = choice.delta else { continue };
                // Generation is what is being timed, and it starts when the
                // first delta of any kind lands — text, thinking, or the first
                // shard of a tool call. Usage-only chunks carry no delta and
                // so never open the window.
                started.get_or_insert_with(Instant::now);
                if let Some(fragment) = &delta.reasoning_content {
                    ui.reasoning_delta(fragment);
                }
                if let Some(fragment) = &delta.content {
                    ui.content_delta(fragment);
                }
                accumulator.feed(&delta);
            }
            // Usage rides on the last content block, so the last one reported wins.
            if chunk.usage.is_some() {
                usage = chunk.usage;
            }
        }
        ui.finish_turn();
        Ok(Reply {
            message: accumulator.finish(),
            usage,
            stream_time: started.map(|s| s.elapsed()).unwrap_or_default(),
            finish_reason,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply_stopped_by(reason: Option<&str>) -> Reply {
        Reply {
            message: Message::default(),
            usage: None,
            stream_time: Duration::default(),
            finish_reason: reason.map(str::to_owned),
        }
    }

    #[test]
    fn only_a_cut_off_answer_owes_a_notice() {
        // The ceiling, in either wire's word for it, and the context window
        // running out: the three ways an answer arrives incomplete.
        for cut in ["length", "max_tokens", "model_context_window_exceeded"] {
            let notice = reply_stopped_by(Some(cut))
                .truncation()
                .unwrap_or_else(|| panic!("{cut} is an answer cut off"));
            assert!(notice.contains("incomplete"), "{notice}");
        }
        // Every other reason is an answer that is whole, and a stream that
        // said nothing is not a stream to invent a failure for.
        for whole in [
            None,
            Some("stop"),
            Some("tool_calls"),
            Some("end_turn"),
            Some("tool_use"),
            Some("stop_sequence"),
            Some("refusal"),
        ] {
            assert!(
                reply_stopped_by(whole).truncation().is_none(),
                "{whole:?} is not a truncation"
            );
        }
    }

    #[test]
    fn the_ceiling_notice_names_the_field_that_was_hit() {
        assert!(
            reply_stopped_by(Some("max_tokens"))
                .truncation()
                .unwrap()
                .contains("max_tokens")
        );
        assert!(
            reply_stopped_by(Some("model_context_window_exceeded"))
                .truncation()
                .unwrap()
                .contains("context window")
        );
    }
}
