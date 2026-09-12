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
//! # The two rules
//!
//! - **Standard in what it sends.** Every field serialized here is a field the
//!   spec describes, spelled as the spec spells it. A field a backend cannot
//!   take is the *caller's* to leave out — a request builder that always sends
//!   everything would make one backend's rejection every backend's.
//! - **Tolerant in what it reads.** The spec adds event types and enum values
//!   over time and tells clients to handle the unknown ones gracefully, so an
//!   event this crate does not know is carried as [`Event::Unknown`] rather than
//!   failing the stream. The one exception is an `error` event, which ends the
//!   stream as an [`Error::Stream`]: a stream that reported its own failure must
//!   never be read as an answer.
//!
//! # Shape
//!
//! A `Client` is built from a `Profile` — where the endpoint is, how to
//! authenticate, which API version to name. Nothing is sent until a request is
//! handed to it, and building either one cannot fail.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod error;
pub mod types;

pub use error::{Api, Error, StreamError};
pub use types::{
    Block, BlockDelta, BlockKind, CacheControl, CacheCreation, CacheType, Effort, Event,
    ImageSource, Message, MessageContent, MessageDeltaBody, MessageParam, MessagesRequest,
    Metadata, OutputConfig, OutputTokensDetails, Role, StopReason, SystemPrompt, ThinkingConfig,
    ThinkingDisplay, Tool, ToolChoice, ToolResultContent, Ttl, Usage,
};
