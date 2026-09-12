//! What can go wrong between a request and a reply.
//!
//! Divided by where the failure came from, not by which call saw it: a caller
//! that wants to retry reads [`Error::is_transient`], one that wants to show the
//! backend's own words prints the error whole — every variant's [`Display`] is
//! written to be read by a person.

use std::fmt;
use std::time::Duration;

use serde::Deserialize;

/// The endpoint refused the request with a non-success status.
///
/// The body is carried verbatim rather than summarized: on this API it names
/// the field that was wrong, and a paraphrase would throw away the only part
/// worth reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Api {
    /// The HTTP status the endpoint answered with.
    pub status: u16,
    /// The response body, as it arrived.
    pub body: String,
    /// The endpoint's own id for this request, from its `x-request-id` header.
    /// The one thing that ties a failure to the endpoint's logs — worth putting
    /// in front of whoever has to look it up.
    pub request_id: Option<String>,
    /// What the endpoint said about trying again, from its `x-should-retry`
    /// header. Not a standard header, and its word outranks the status: an
    /// endpoint may know that a status this client would retry is permanent
    /// here, and that one it would write off is worth another attempt.
    pub should_retry: Option<bool>,
    /// How long the endpoint asked to be left alone, from `retry-after-ms` or
    /// `retry-after`. `None` when it asked for nothing, or asked for a wait no
    /// client should obey.
    pub retry_after: Option<Duration>,
}

impl Api {
    /// Whether another attempt is worth making.
    ///
    /// What the endpoint said, when it said anything; otherwise the statuses the
    /// reference client retries: a request that timed out, a lock that did, a
    /// rate limit, and every 5xx.
    pub fn is_transient(&self) -> bool {
        if let Some(should_retry) = self.should_retry {
            return should_retry;
        }
        matches!(self.status, 408 | 409 | 429) || self.status >= 500
    }

    /// What kind of refusal this is, in the vocabulary the reference client's
    /// exceptions use.
    ///
    /// Its own word first: the reference maps these statuses to classes by hand
    /// (`400 → BadRequestError`, `422 → UnprocessableEntityError`, every 5xx →
    /// `InternalServerError`, everything else → the generic `APIStatusError`),
    /// and a caller that branches on the class there should branch on the same
    /// class here. The rest of the variants are the failures that have no
    /// status: a connection, a timeout, a payload that is not the spec, a client
    /// built wrong, a stream that reported its own failure.
    #[allow(clippy::manual_range_contains)]
    pub fn kind(&self) -> Kind {
        match self.status {
            400 => Kind::BadRequest,
            401 => Kind::Authentication,
            403 => Kind::Permission,
            404 => Kind::NotFound,
            409 => Kind::Conflict,
            422 => Kind::Unprocessable,
            429 => Kind::RateLimit,
            status if status >= 500 => Kind::ServerError,
            status => Kind::OtherStatus(status),
        }
    }
}

/// What kind of failure this is, in the vocabulary the reference client's
/// exceptions use.
///
/// Its own word first: the reference maps these statuses to classes by hand and
/// a caller that branches on the class there should branch on the same class
/// here. The rest of the variants are the failures that have no status: a
/// connection, a timeout, a payload that is not the spec, a client built wrong,
/// a stream that reported its own failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// 400
    BadRequest,
    /// 401
    Authentication,
    /// 403
    Permission,
    /// 404
    NotFound,
    /// 409
    Conflict,
    /// 422
    Unprocessable,
    /// 429
    RateLimit,
    /// Any other 5xx.
    ServerError,
    /// Any other status the endpoint refused with.
    OtherStatus(u16),
    /// The request never arrived: DNS, TLS, a refused connection.
    Connection,
    /// The request arrived and the answer did not come back in time.
    Timeout,
    /// A payload that is not the shape the spec describes.
    Decode,
    /// The client was built around something that cannot work.
    Config,
    /// The stream reported its own failure.
    Stream,
}

impl fmt::Display for Api {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "API returned HTTP {}\nresponse body: {}",
            self.status, self.body
        )?;
        // The endpoint's own id, where it named one: it is what a support
        // conversation about this failure starts with.
        if let Some(id) = &self.request_id {
            write!(f, "\nrequest id: {id}")?;
        }
        Ok(())
    }
}

/// A failure the stream reported in a chunk whose payload named an `error`.
///
/// The reference client's own shape: a code, a message, and a type, all of
/// which the body keeps verbatim. A stream that carries one is over — there is
/// no more of the answer coming, so a caller must not finish a turn from what
/// arrived before it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct StreamError {
    /// The error code the endpoint named, when it named one.
    #[serde(default)]
    pub code: Option<String>,
    /// The message, as the endpoint wrote it.
    pub message: String,
    /// The error type the endpoint named, when it named one.
    #[serde(default)]
    pub r#type: Option<String>,
    /// The parameter the endpoint blamed, when it blamed one.
    #[serde(default)]
    pub param: Option<String>,
}

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(code) = &self.code {
            write!(f, "stream error {code}")?;
        } else {
            write!(f, "stream error")?;
        }
        if let Some(kind) = &self.r#type {
            write!(f, " ({kind})")?;
        }
        write!(f, ": {}", self.message)?;
        if let Some(param) = &self.param {
            write!(f, " (param: {param})")?;
        }
        Ok(())
    }
}

/// Everything this crate fails with.
#[derive(Debug)]
pub enum Error {
    /// The client was built around something that cannot work — an endpoint
    /// URL that does not parse. Nothing was sent, and nothing will be until the
    /// configuration is fixed.
    Config(String),
    /// The endpoint answered with a status that is not a success.
    Api(Api),
    /// The request never completed: DNS, TLS, connection, timeout.
    Transport(reqwest::Error),
    /// A payload that is not the shape the spec describes. `what` names which
    /// one, because a failure to read an event and a failure to read a message
    /// are worth telling apart in a log.
    Decode {
        /// Which payload failed to decode.
        what: &'static str,
        /// What serde said.
        source: serde_json::Error,
    },
    /// The stream reported its own failure and ended.
    Stream(StreamError),
}

impl Error {
    /// Whether trying the same request again could work. See [`Api::is_transient`]
    /// for the status policy and [`Error::kind`] for what the failure was.
    pub fn is_transient(&self) -> bool {
        match self {
            Error::Transport(_) => true,
            Error::Api(api) => api.is_transient(),
            // A stream error is the backend failing mid-answer: the same
            // request may well go through next time.
            Error::Stream(_) => true,
            Error::Decode { .. } | Error::Config(_) => false,
        }
    }

    /// What kind of failure this is. See [`Kind`].
    pub fn kind(&self) -> Kind {
        match self {
            Error::Api(api) => api.kind(),
            Error::Stream(_) => Kind::Stream,
            Error::Decode { .. } => Kind::Decode,
            Error::Config(_) => Kind::Config,
            // A transport failure is a timeout or it is not, and the transport
            // is the only thing that can tell: a request that timed out was
            // heard by somebody, and one that never connected was not.
            Error::Transport(e) if e.is_timeout() => Kind::Timeout,
            Error::Transport(_) => Kind::Connection,
        }
    }

    /// How long the endpoint asked to be left alone before another attempt, when
    /// it asked. See [`Api::retry_after`].
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Error::Api(api) => api.retry_after,
            _ => None,
        }
    }

    /// The endpoint's id for the request that failed, when it named one.
    pub fn request_id(&self) -> Option<&str> {
        match self {
            Error::Api(api) => api.request_id.as_deref(),
            _ => None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Config(msg) => write!(f, "invalid client configuration: {msg}"),
            Error::Api(api) => api.fmt(f),
            Error::Transport(e) => write!(f, "request failed: {e}"),
            Error::Decode { what, source } => write!(f, "failed to parse {what}: {source}"),
            Error::Stream(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Transport(e) => Some(e),
            Error::Decode { source, .. } => Some(source),
            Error::Api(_) | Error::Stream(_) | Error::Config(_) => None,
        }
    }
}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        Error::Transport(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A refusal as the endpoint made it, with only the status of interest.
    fn refused(status: u16) -> Api {
        Api {
            status,
            body: String::new(),
            request_id: None,
            should_retry: None,
            retry_after: None,
        }
    }

    #[test]
    fn an_api_refusal_shows_the_status_and_the_backends_own_words() {
        let err = Error::Api(Api {
            body:
                r#"{"error":{"message":"invalid","type":"invalid_request_error","code":"invalid"}}"#
                    .into(),
            ..refused(400)
        });
        let shown = err.to_string();
        assert!(shown.contains("400"), "{shown}");
        assert!(shown.contains("invalid_request_error"), "{shown}");
    }

    #[test]
    fn a_stream_error_names_its_code_message_and_param() {
        let err = Error::Stream(StreamError {
            code: Some("invalid_api_key".into()),
            message: "Incorrect API key provided".into(),
            r#type: Some("invalid_request_error".into()),
            param: None,
        });
        let shown = err.to_string();
        assert!(shown.contains("invalid_api_key"), "{shown}");
        assert!(shown.contains("invalid_request_error"), "{shown}");
        assert!(shown.contains("Incorrect API key"), "{shown}");
    }

    #[test]
    fn a_stream_error_with_no_code_or_type_still_names_the_message() {
        let err = Error::Stream(StreamError {
            code: None,
            message: "Overloaded".into(),
            r#type: None,
            param: None,
        });
        let shown = err.to_string();
        assert!(shown.contains("Overloaded"), "{shown}");
        assert!(!shown.contains("()"), "{shown}");
    }

    #[test]
    fn a_decode_failure_names_the_payload_it_was_reading() {
        let source = serde_json::from_str::<serde_json::Value>("{oops").unwrap_err();
        let err = Error::Decode {
            what: "SSE chunk",
            source,
        };
        assert!(err.to_string().contains("SSE chunk"), "{err}");
    }

    #[test]
    fn only_a_failure_worth_retrying_is_transient() {
        // The endpoint asking for another moment (a rate limit, a lock or
        // request that timed out), or the backend failing mid-answer: the same
        // request may go through.
        for status in [408, 409, 429, 500, 503, 504] {
            assert!(Error::Api(refused(status)).is_transient(), "HTTP {status}");
        }
        // A request the endpoint will refuse identically, and a payload that
        // is not the spec: retrying changes nothing.
        for status in [400, 401, 403, 404, 422] {
            assert!(!Error::Api(refused(status)).is_transient(), "HTTP {status}");
        }
        let source = serde_json::from_str::<serde_json::Value>("{oops").unwrap_err();
        assert!(!Error::Decode { what: "x", source }.is_transient());
        assert!(
            Error::Stream(StreamError {
                code: None,
                message: "Overloaded".into(),
                r#type: None,
                param: None,
            })
            .is_transient()
        );
    }

    #[test]
    fn what_the_endpoint_says_about_retrying_outranks_its_status() {
        // The header is not standard and it is the endpoint's own answer: a
        // status this client would retry is not worth one here, and one it would
        // write off is.
        assert!(
            !Api {
                should_retry: Some(false),
                ..refused(429)
            }
            .is_transient()
        );
        assert!(
            Api {
                should_retry: Some(true),
                ..refused(400)
            }
            .is_transient()
        );
    }

    #[test]
    fn every_status_has_the_kind_the_reference_client_would_have_given_it() {
        // The reference maps statuses to exception classes by hand, and a caller
        // branching on the class there branches on the same one here.
        for (status, kind) in [
            (400, Kind::BadRequest),
            (401, Kind::Authentication),
            (403, Kind::Permission),
            (404, Kind::NotFound),
            (409, Kind::Conflict),
            (422, Kind::Unprocessable),
            (429, Kind::RateLimit),
            (500, Kind::ServerError),
            (503, Kind::ServerError),
            (504, Kind::ServerError),
            (529, Kind::ServerError),
            (402, Kind::OtherStatus(402)),
            (408, Kind::OtherStatus(408)),
        ] {
            assert_eq!(Error::Api(refused(status)).kind(), kind, "HTTP {status}");
        }
        // The failures that have no status of their own.
        let source = serde_json::from_str::<serde_json::Value>("{oops").unwrap_err();
        assert_eq!(Error::Decode { what: "x", source }.kind(), Kind::Decode);
        assert_eq!(Error::Config("bad".into()).kind(), Kind::Config);
        assert_eq!(
            Error::Stream(StreamError {
                code: None,
                message: "Overloaded".into(),
                r#type: None,
                param: None,
            })
            .kind(),
            Kind::Stream
        );
    }

    #[test]
    fn a_refusal_shows_the_id_the_endpoint_gave_the_request() {
        let err = Error::Api(Api {
            request_id: Some("req_01".into()),
            ..refused(500)
        });
        let shown = err.to_string();
        assert!(shown.contains("req_01"), "{shown}");
        // And a refusal nobody named shows no id, rather than an empty one.
        assert!(!Error::Api(refused(500)).to_string().contains("request id"));
    }

    #[test]
    fn the_requests_own_id_is_reachable() {
        let err = Error::Api(Api {
            request_id: Some("req_1".into()),
            ..refused(500)
        });
        assert_eq!(err.request_id(), Some("req_1"));
        assert_eq!(err.retry_after(), None);
        assert_eq!(Error::Config("x".into()).request_id(), None);
    }

    #[test]
    fn a_misconfigured_client_is_not_worth_retrying() {
        let err = Error::Config("endpoint URL does not parse: \"ht tp://x\"".into());
        assert!(!err.is_transient());
        assert!(err.to_string().contains("ht tp://x"), "{err}");
        assert!(std::error::Error::source(&err).is_none());
    }

    #[test]
    fn the_transport_failure_stays_reachable() {
        // A request that never arrived is the one a caller may want to inspect,
        // so it is the one whose cause must come back out of `source`.
        let source = reqwest::Client::new()
            .get("ht tp://not a url")
            .build()
            .expect_err("a URL with a space in the scheme cannot be built");
        let e = Error::from(source);
        assert!(matches!(e, Error::Transport(_)));
        assert!(std::error::Error::source(&e).is_some());
        assert!(
            e.is_transient(),
            "a request that never arrived is worth retrying"
        );
    }
}
