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
        while let Some(chunk) = stream.next_chunk().await? {
            for choice in chunk.choices {
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
        })
    }
}
