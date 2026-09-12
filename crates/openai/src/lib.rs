//! A client for two OpenAI HTTP surfaces: the **Chat Completions** API and
//! the **Responses** API.
//!
//! This crate is two standard wires, each as types and a stream:
//! `POST /v1/chat/completions` with the SSE chunk stream it answers with, and
//! `POST /v1/responses` with the SSE event stream it answers with. Both are
//! written to the published spec rather than to one backend's behaviour, so an
//! endpoint that implements the spec works through either — `api.openai.com`
//! and an OpenAI-compatible gateway alike — and a divergence has one place to
//! live: [`Profile`].
//!
//! # What it knows, and what it does not
//!
//! The crate knows the protocol: what a request body may carry, what a chunk
//! or event means, what a `finish_reason` or `status` says, and what the
//! counts in `usage` are. It does not know the caller: no prompt, no history,
//! no tools, no policy about what to do with a truncated answer. Those
//! belong to the layer above, which is why nothing here reads a file, an
//! environment variable or a clock.
//!
//! # What it is measured against
//!
//! The reference client for these APIs is `openai-python`, and this crate
//! exists to behave like it: the fields it sends, the chunks and events it
//! hands over, the failures it retries, the bounds it puts on a slow endpoint.
//! When the two disagree about a behavior, one of them is wrong and the
//! difference is a bug to settle — not a preference to keep. The few places
//! this crate differs on purpose are listed below, and a change that adds
//! another should say why here.
//!
//! # The two rules
//!
//! - **Standard in what it sends.** Every field serialized here is a field the
//!   spec describes, spelled as the spec spells it. A field a backend cannot
//!   take is the *caller's* to leave out — a request builder that always sends
//!   everything would make one backend's rejection every backend's.
//! - **Tolerant in what it reads.** The spec adds enum values over time and
//!   tells clients to handle the unknown ones gracefully, so [`FinishReason`],
//!   [`ResponseStatus`] and the service-tier enums keep the wire string when
//!   they don't know it, and unknown object fields are ignored rather than
//!   refused. The same rule holds for the framing: a line may end any of the
//!   three ways the SSE spec allows, a frame's payload may be spread over
//!   several `data:` lines, and a frame that carried no data at all is read
//!   past, since the reference client's own decoder does the same.
//!
//! # Where it differs from the reference client, on purpose
//!
//! Three things, and nothing else a caller can observe:
//!
//! - **The known-tiers are a strict subset.** The reference client sends
//!   `X-Stainless-*` headers for diagnostics; this crate sends only what the
//!   request itself needs (`accept`, `content-type`, `user-agent`,
//!   `authorization`, and `OpenAI-Organization` / `OpenAI-Project` when set).
//!   The diagnostics ride on a per-deployment observability layer; the wire
//!   protocol is the same either way.
//! - **A payload is refused when reading it would mean reading it *wrongly*.**
//!   The reference client constructs an answer leniently everywhere: a field
//!   the spec requires and the endpoint left out is simply absent on the
//!   object. This crate draws the line at what a missing field would cost —
//!   an id, a count, a message's role defaults, because the worst that comes
//!   of it is reading *less* than the endpoint sent, while the fields that
//!   tie a chunk to its place in the stream stay required, because guessing
//!   them would quietly shard one chunk's text, or one call's arguments,
//!   onto another.
//! - **A stalled write has no bound.** The reference client's transport
//!   bounds reads, writes and the pool at ten minutes and the connect at
//!   five; reqwest exposes a read timeout and a connect timeout and no more,
//!   so those two are set and the other two cannot be. A request body that
//!   stops moving mid-write is not something this crate can give up on.
//! - **Nothing reconnects.** The SSE spec's `id` and `retry` fields are read
//!   past rather than kept: a client that resumes a stream is a client with
//!   state, and the reference client does not resume one either.
//!
//! # Shape
//!
//! ```
//! // The crate's own name; a consumer may rename it in its manifest.
//! use caocli_openai::{
//!     ChatCompletionRequest, ChatCompletionMessageParam, Client, Profile,
//!     FunctionDefinition, Tool, UserMessageParam,
//! };
//!
//! let profile = Profile::base("https://api.openai.com")
//!     .with_bearer_token("sk-…");
//! let client = Client::new(profile).expect("the endpoint parses");
//!
//! let request = ChatCompletionRequest::streaming(
//!     "gpt-4o",
//!     vec![ChatCompletionMessageParam::User(
//!         UserMessageParam::new("say hello"),
//!     )],
//! )
//! .with_max_tokens(1024)
//! .with_tool(Tool::function(FunctionDefinition::new(
//!     "get_weather",
//!     serde_json::json!({"type": "object"}),
//! )));
//!
//! // let mut stream = client.stream_completion(&request).await?;
//! // while let Some(chunk) = stream.next_chunk().await? { … }
//! ```
//!
//! Nothing is sent until [`Client::completion`], [`Client::stream_completion`],
//! [`Client::responses`] or [`Client::stream_responses`] is called. The only
//! way building a client fails is the endpoint: a URL that does not parse is
//! [`Error::Config`], because it is a configuration mistake and not a network
//! one.
//!
//! # Two surfaces
//!
//! The crate speaks two of OpenAI's HTTP surfaces:
//!
//! - **Chat Completions** at `POST /v1/chat/completions`: [`Client::completion`]
//!   and [`Client::stream_completion`], and the [`types`] module.
//! - **Responses** at `POST /v1/responses`: [`Client::responses`] and
//!   [`Client::stream_responses`], and the [`responses_types`] module.
//!
//! One [`Profile`] points at a single base URL; its methods append the right
//! path per surface — a caller that names `https://api.openai.com/v1` can call
//! both surfaces through one [`Client`].

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod client;
pub mod error;
pub mod responses_stream;
pub mod responses_types;
pub mod stream;
pub mod types;

pub use client::{Auth, Client, DEFAULT_MAX_RETRIES, Profile};
pub use error::{Api, Error, Kind, StreamError};
pub use responses_stream::{
    ByteStream as ResponsesByteStream, ReasoningSummaryPart, ResponseEventStream,
    ResponseStreamEvent, ResponseUsageSummary, StreamContentPart,
};
pub use responses_types::{
    ApplyPatchChoice, ApplyPatchTag, Conversation, EasyInputMessage, FunctionCallOutputItem,
    IncompleteDetails, InputMessageRole, InputTokensDetails, ItemReference, ItemStatus,
    McpToolDiscriminator, MessagePhase, OutputMessageContent, OutputTokensDetails,
    ProgrammaticToolCallingChoice, ProgrammaticToolCallingTag, PromptCacheDiagnostics,
    ReasoningConfig, ReasoningEffort as ResponsesReasoningEffort, ReasoningItemInput,
    ReasoningSummaryText, ReasoningTextContent, ReasoningTextKind, Response,
    ResponseAllowedToolMode, ResponseAllowedToolRef, ResponseBuiltInToolKind,
    ResponseCreateRequest, ResponseError, ResponseFunctionTool, ResponseFunctionToolCall,
    ResponseImageDetail, ResponseInput, ResponseInputContent, ResponseInputContentPart,
    ResponseInputItem, ResponseInputItemType, ResponseModeration, ResponseModerationConfig,
    ResponseModerationMode, ResponseModerationPolicy, ResponseModerationScored,
    ResponseModerationSide, ResponseOutputItem, ResponseOutputMessage, ResponsePromptCacheOptions,
    ResponseReasoningItem, ResponseServiceTierOrUnknown, ResponseStatus, ResponseTextFormat,
    ResponseTool, ResponseToolChoice, ResponseToolChoiceAllowed, ResponseToolChoiceAllowedType,
    ResponseToolChoiceCustom, ResponseToolChoiceFunction, ResponseToolChoiceMcp,
    ResponseToolChoiceMode, ResponseToolChoiceTypes, ResponseToolType,
    ResponsesCustomToolDiscriminator, ServiceTier as ResponsesServiceTier, ShellChoice, ShellTag,
    StreamOptions as ResponsesStreamOptions, TextConfig, Truncation,
};
pub use stream::{ByteStream, ChunkStream};
pub use types::{
    Annotation, AssistantMessageParam, AudioConfig, AudioFormat, ChatCompletion,
    ChatCompletionAudio, ChatCompletionChunk, ChatCompletionMessage, ChatCompletionMessageParam,
    ChatCompletionRequest, ChatCompletionTokenLogprob, Choice, ChoiceDelta, ChoiceLogprobs,
    ChunkChoice, CompletionTokensDetails, CompletionUsage, ContentPart, DeltaFunctionCall,
    DeltaToolCall, DeveloperMessageParam, ErrorBody, ErrorObject, FinishReason, FunctionCall,
    FunctionCallMode, FunctionCallOption, FunctionDefinition, FunctionMessageParam, FunctionTool,
    FunctionToolCall, ImageDetail, ImageUrl, JsonSchemaSpec, MessageContent, MessageToolCall,
    Metadata, Modality, ModerationConfig, ModerationMode, ModerationPolicy, ModerationPolicyInput,
    ModerationPolicyOutput, ModerationResult, ModerationScored, ModerationSide, NamedFunction,
    NamedToolChoice, PredictionConfig, PredictionContent, PredictionType, PromptCacheMode,
    PromptCacheOptions, PromptCacheRetention, PromptCacheTtl, PromptTokensDetails,
    ReasoningEffort as ChatReasoningEffort, ResponseFormat, Role, SearchContextSize, ServiceTier,
    ServiceTierOrUnknown, Stop, StreamOptions as ChatStreamOptions, SystemMessageParam, Tool,
    ToolChoice, ToolChoiceMode, ToolMessageParam, ToolType, TopLogprob, UrlCitation, UserLocation,
    UserMessageParam, Verbosity, Voice, WebSearchOptions,
};
