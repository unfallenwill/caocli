//! A client for the Anthropic Messages API.
//!
//! This is the standard wire, as types and a stream: `POST /v1/messages`, the
//! request body the spec describes, and the named event stream it answers with.
//! It is written to the published spec rather than to one backend's behaviour,
//! so an endpoint that implements the spec works through it — `api.anthropic.com`
//! and an Anthropic-compatible gateway alike — and a divergence has one place to
//! live: [`Profile`].
//!
//! # What it knows, and what it does not
//!
//! The crate knows the protocol: what a request body may carry, what the events
//! mean, what a `stop_reason` says, and what the counts in `usage` are. It does
//! not know the caller: no prompt, no history, no tools, no policy about what to
//! do with a truncated answer. Those belong to the layer above, which is why
//! nothing here reads a file, an environment variable or a clock.
//!
//! # What it is measured against
//!
//! The reference client for this API is `anthropic-sdk-python`, and this crate
//! exists to behave like it: the fields it sends, the events it hands over, the
//! failures it retries, the bounds it puts on a slow endpoint. When the two
//! disagree about a behavior, one of them is wrong and the difference is a bug to
//! settle — not a preference to keep. The few places this crate differs on
//! purpose are listed below, and a change that adds another should say why here.
//!
//! # The two rules
//!
//! - **Standard in what it sends.** Every field serialized here is a field the
//!   spec describes, spelled as the spec spells it. A field a backend cannot
//!   take is the *caller's* to leave out — a request builder that always sends
//!   everything would make one backend's rejection every backend's.
//! - **Tolerant in what it reads.** The spec adds event types and enum values
//!   over time and tells clients to handle the unknown ones gracefully, so an
//!   event this crate does not know — and a `ping` keep-alive, which carries
//!   nothing — is read past rather than failing the stream, and handed to nobody.
//!   The same rule holds for the framing: an event's type may be spelled only in
//!   the SSE name, an event's payload may be spread over several `data:` lines,
//!   and a line may end any of the three ways the spec allows. The one thing that
//!   is not tolerated is an `error` event, which ends the stream as an
//!   [`Error::Stream`]: a stream that reported its own failure must never be read
//!   as an answer.
//!
//! # Where it differs from the reference client, on purpose
//!
//! Three things, and nothing else a caller can observe:
//!
//! - **`usage` counts stay optional.** The reference marks the prompt and the
//!   answer count as required, so a stream that omits one fails its validation.
//!   Here every count is an [`Option`], which is what the merging of a stream
//!   needs: an event that carries a count replaces it, one that does not leaves
//!   it alone. An endpoint that reports zeros in one event and the truth in
//!   another — the shape this wire actually arrives in — is then read rather than
//!   refused. The one place a caller sees a difference in tolerance, and it is in
//!   the direction of reading more streams, not fewer.
//! - **The fields of a *known* object are not kept.** A model in the reference has
//!   room for fields it does not know; here they are ignored. The exception is a
//!   whole block whose *kind* is unknown, which keeps the object it arrived in —
//!   see [`Block::unmodelled`].
//! - **Nothing reconnects.** The spec's `id` and `retry` SSE fields are read past
//!   rather than kept: a client that resumes a stream is a client with state, and
//!   the reference client does not resume one either.
//!
//! # Shape
//!
//! ```
//! // The crate's own name; a consumer may rename it in its manifest, and the
//! // agent here does — it depends on it as `anthropic`.
//! use caocli_anthropic::{Client, MessageParam, MessagesRequest, Profile};
//!
//! let profile = Profile::base("https://api.anthropic.com")
//!     .with_bearer_token("sk-ant-…")
//!     .with_beta("interleaved-thinking-2025-05-14");
//! let client = Client::new(profile).expect("the endpoint parses");
//!
//! let request = MessagesRequest::new(
//!     "claude-sonnet-5",
//!     4096,
//!     vec![MessageParam::user("say hello")],
//! )
//! .streaming()
//! .with_automatic_cache();
//!
//! // let mut events = client.stream(&request).await?;
//! // while let Some(event) = events.next_event().await? { … }
//! ```
//!
//! Nothing is sent until [`Client::messages`] or [`Client::stream`] is called.
//! The only way building a client fails is the endpoint: a URL that does not
//! parse is [`Error::Config`], because it is a configuration mistake and not a
//! network one.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod client;
pub mod error;
pub mod stream;
pub mod types;

pub use client::{API_VERSION, Auth, Client, Profile};
pub use error::{Api, Error, StreamError};
pub use stream::EventStream;
pub use types::{
    Block, BlockDelta, BlockKind, CacheControl, CacheCreation, CacheType, Effort, Event,
    ImageSource, Message, MessageContent, MessageDeltaBody, MessageParam, MessagesRequest,
    Metadata, OutputConfig, OutputTokensDetails, Role, StopReason, SystemPrompt, ThinkingConfig,
    ThinkingDisplay, Tool, ToolChoice, ToolResultContent, Ttl, Usage,
};
