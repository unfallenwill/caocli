//! What can go wrong between a request and a reply.
//!
//! Divided by where the failure came from, not by which call saw it: a caller
//! that wants to retry reads [`Error::is_transient`], one that wants to show the
//! backend's own words prints the error whole — every variant's [`Display`] is
//! written to be read by a person.

use std::fmt;

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
}

impl fmt::Display for Api {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "API returned HTTP {}\nresponse body: {}",
            self.status, self.body
        )
    }
}

/// A failure the stream reported in an `error` event.
///
/// The spec's own body: a type such as `overloaded_error` and a message meant to
/// be shown. A stream that carries one is over — there is no more of the answer
/// coming, so a caller must not finish a turn from what arrived before it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct StreamError {
    /// The error type the endpoint named (`overloaded_error`, `api_error`, …).
    pub r#type: String,
    /// The message, as the endpoint wrote it.
    pub message: String,
}

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "stream error {}: {}", self.r#type, self.message)
    }
}

/// Everything this crate fails with.
#[derive(Debug)]
pub enum Error {
    /// The client was built around something that cannot work — an endpoint
    /// URL that does not parse. Nothing was sent, and nothing will be until
    /// the configuration is fixed.
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
    /// Whether trying the same request again could work.
    ///
    /// A refusal with a 4xx status is the request's own fault and will fail
    /// identically; 429 and 5xx are the endpoint asking for another moment,
    /// and a transport failure never arrived at all. A misconfigured client is
    /// not transient either: the same client sends the same wrong request.
    pub fn is_transient(&self) -> bool {
        match self {
            Error::Transport(_) => true,
            Error::Api(api) => api.status == 429 || api.status >= 500,
            // A stream error is the backend failing mid-answer: the same
            // request may well go through next time. The spec's own advice for
            // `overloaded_error` is to retry.
            Error::Stream(_) => true,
            Error::Decode { .. } | Error::Config(_) => false,
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

    #[test]
    fn an_api_refusal_shows_the_status_and_the_backends_own_words() {
        let err = Error::Api(Api {
            status: 400,
            body: r#"{"type":"error","error":{"type":"invalid_request_error"}}"#.into(),
        });
        let shown = err.to_string();
        assert!(shown.contains("400"), "{shown}");
        assert!(shown.contains("invalid_request_error"), "{shown}");
    }

    #[test]
    fn a_stream_error_names_its_type_and_message() {
        let err = Error::Stream(StreamError {
            r#type: "overloaded_error".into(),
            message: "Overloaded".into(),
        });
        let shown = err.to_string();
        assert!(shown.contains("overloaded_error"), "{shown}");
        assert!(shown.contains("Overloaded"), "{shown}");
    }

    #[test]
    fn a_decode_failure_names_the_payload_it_was_reading() {
        let source = serde_json::from_str::<serde_json::Value>("{oops").unwrap_err();
        let err = Error::Decode {
            what: "SSE event",
            source,
        };
        assert!(err.to_string().contains("SSE event"), "{err}");
    }

    #[test]
    fn only_a_failure_worth_retrying_is_transient() {
        // The endpoint asking for another moment, or the backend failing
        // mid-answer: the same request may go through.
        for status in [429, 500, 529] {
            assert!(
                Error::Api(Api {
                    status,
                    body: String::new()
                })
                .is_transient(),
                "HTTP {status}"
            );
        }
        // A request the endpoint will refuse identically, and a payload that
        // is not the spec: retrying changes nothing.
        for status in [400, 401, 404, 413] {
            assert!(
                !Error::Api(Api {
                    status,
                    body: String::new()
                })
                .is_transient(),
                "HTTP {status}"
            );
        }
        let source = serde_json::from_str::<serde_json::Value>("{oops").unwrap_err();
        assert!(!Error::Decode { what: "x", source }.is_transient());
        assert!(
            Error::Stream(StreamError {
                r#type: "overloaded_error".into(),
                message: "Overloaded".into(),
            })
            .is_transient()
        );
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
