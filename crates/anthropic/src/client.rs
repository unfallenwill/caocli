//! The client: where the endpoint is, how to authenticate, and the two calls
//! the API has.
//!
//! Everything an endpoint may differ in is a field of [`Profile`] and nothing
//! else in this crate is: the path, the version header, which header carries the
//! credential, the beta names. A backend that speaks the spec is a `Profile`,
//! not a branch.

use std::time::Duration;

use crate::error::{Api, Error};
use crate::stream::EventStream;
use crate::types::{Message, MessagesRequest};

/// The path the Messages API answers at, under a base URL.
const MESSAGES_PATH: &str = "/v1/messages";

/// The API version this crate speaks. Requests name it — the header is
/// required, and a request without one is refused rather than defaulted.
pub const API_VERSION: &str = "2023-06-01";

/// How the client proves who it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Auth {
    /// `Authorization: Bearer <token>`: the primary form, and the one a
    /// short-lived token from a federation exchange takes.
    Bearer(String),
    /// `x-api-key: <key>`: the legacy fallback, still served.
    ApiKey(String),
    /// Nothing. For an endpoint that authenticates some other way — one behind
    /// a proxy, or one that asks for no key at all.
    None,
}

/// Where the messages go, and who is sending them.
///
/// Built rather than assembled, because the two ways to name an endpoint are
/// not the same thing: [`Profile::base`] takes the base the spec's own
/// convention appends `/v1/messages` to, and [`Profile::endpoint`] takes the
/// full URL for a gateway that puts the path somewhere else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    endpoint: String,
    api_version: String,
    auth: Auth,
    betas: Vec<String>,
}

impl Profile {
    /// A profile for a base URL, with the standard path under it.
    pub fn base(base: impl Into<String>) -> Self {
        let base = base.into();
        let endpoint = format!("{}{}", base.trim_end_matches('/'), MESSAGES_PATH);
        Self::endpoint(endpoint)
    }

    /// A profile for an endpoint named in full, path and all.
    pub fn endpoint(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            api_version: API_VERSION.to_string(),
            auth: Auth::None,
            betas: Vec::new(),
        }
    }

    /// Authenticate with `Authorization: Bearer`, the primary form.
    pub fn with_bearer_token(mut self, token: impl Into<String>) -> Self {
        self.auth = Auth::Bearer(token.into());
        self
    }

    /// Authenticate with `x-api-key`, the legacy fallback.
    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.auth = Auth::ApiKey(key.into());
        self
    }

    /// Name a different API version. There is exactly one, so this is for a
    /// test or a gateway that insists on its own date.
    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.api_version = version.into();
        self
    }

    /// Send to a different endpoint instead, everything else unchanged — a
    /// proxy in front of the API, or the endpoint of a test.
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    /// Send a beta name in the `anthropic-beta` header. Several names ride in
    /// one header, comma-separated.
    pub fn with_beta(mut self, beta: impl Into<String>) -> Self {
        self.betas.push(beta.into());
        self
    }

    /// The endpoint a request will be sent to.
    pub fn url(&self) -> &str {
        &self.endpoint
    }
}

/// A connection to one endpoint.
pub struct Client {
    profile: Profile,
    http: reqwest::Client,
}

impl std::fmt::Debug for Client {
    /// The profile, never the credential's value: a key in a log is a key
    /// leaked, and `Debug` is how values reach logs.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("endpoint", &self.profile.endpoint)
            .field("api_version", &self.profile.api_version)
            .field(
                "auth",
                &match &self.profile.auth {
                    Auth::Bearer(_) => "Bearer(…)",
                    Auth::ApiKey(_) => "ApiKey(…)",
                    Auth::None => "None",
                },
            )
            .finish_non_exhaustive()
    }
}

impl Client {
    /// Connect to the endpoint a profile names.
    ///
    /// The only failure is the endpoint: a URL that does not parse cannot be
    /// turned into a request later, and saying so now is the difference between
    /// a configuration error and a network one.
    pub fn new(profile: Profile) -> Result<Self, Error> {
        let parsed = reqwest::Url::parse(&profile.endpoint).map_err(|e| {
            Error::Config(format!(
                "endpoint URL does not parse: {:?} ({e})",
                profile.endpoint
            ))
        })?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(Error::Config(format!(
                "endpoint URL must be http or https, not {:?}",
                parsed.scheme()
            )));
        }
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| Error::Config(format!("failed to build the HTTP client: {e}")))?;
        Ok(Self { profile, http })
    }

    /// The profile this client was built from.
    pub fn profile(&self) -> &Profile {
        &self.profile
    }

    /// One complete answer, delivered at once.
    ///
    /// [`Client::stream`] is the one to prefer for a long answer: the spec's own
    /// guidance is that a large `max_tokens` needs streaming to survive request
    /// timeouts, and an answer that is displayed as it is written is the point
    /// of the call.
    pub async fn messages(&self, request: &MessagesRequest) -> Result<Message, Error> {
        let body = self.body(request)?;
        let response = self.post(body).send().await.map_err(Error::Transport)?;
        let status = response.status();
        let bytes = response.bytes().await.map_err(Error::Transport)?;
        if !status.is_success() {
            return Err(Error::Api(Api {
                status: status.as_u16(),
                body: String::from_utf8_lossy(&bytes).into_owned(),
            }));
        }
        serde_json::from_slice(&bytes).map_err(|source| Error::Decode {
            what: "message",
            source,
        })
    }

    /// An answer as it is written, one event at a time.
    ///
    /// The request is sent as a stream whatever its `stream` field says: the
    /// field is part of the body the endpoint reads, so a caller that asks for
    /// events through this call gets them, and one that wants a whole message
    /// uses [`Client::messages`].
    pub async fn stream(&self, request: &MessagesRequest) -> Result<EventStream, Error> {
        let body = self.body(request)?;
        let response = self
            .post(body)
            .header("accept", "text/event-stream")
            .send()
            .await
            .map_err(Error::Transport)?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(Error::Api(Api {
                status: status.as_u16(),
                body,
            }));
        }
        Ok(EventStream::new(Box::pin(response.bytes_stream())))
    }

    /// The request itself: the profile's headers, and the body verbatim.
    fn post(&self, body: Vec<u8>) -> reqwest::RequestBuilder {
        let mut request = self
            .http
            .post(&self.profile.endpoint)
            .header("content-type", "application/json")
            .header("anthropic-version", &self.profile.api_version)
            .body(body);
        request = match &self.profile.auth {
            Auth::Bearer(token) => request.bearer_auth(token),
            Auth::ApiKey(key) => request.header("x-api-key", key),
            Auth::None => request,
        };
        if !self.profile.betas.is_empty() {
            request = request.header("anthropic-beta", self.profile.betas.join(","));
        }
        request
    }

    fn body(&self, request: &MessagesRequest) -> Result<Vec<u8>, Error> {
        serde_json::to_vec(request).map_err(|source| Error::Decode {
            what: "request",
            source,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{MessageParam, Usage};
    use serde_json::json;
    use wiremock::matchers::{header, headers, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn request() -> MessagesRequest {
        MessagesRequest::new("m", 16, vec![MessageParam::user("hi")]).streaming()
    }

    /// A client for a mock endpoint, which is where every test here sends.
    fn client_for(server: &MockServer, profile: Profile) -> Client {
        Client::new(profile.with_endpoint(server.uri() + "/v1/messages"))
            .expect("the mock URL parses")
    }

    #[test]
    fn a_base_url_gets_the_standard_path_and_a_trailing_slash_is_not_doubled() {
        assert_eq!(
            Profile::base("https://api.anthropic.com").url(),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            Profile::base("https://api.anthropic.com/").url(),
            "https://api.anthropic.com/v1/messages"
        );
        // A gateway that puts the path somewhere else names it in full.
        assert_eq!(
            Profile::endpoint("https://api.minimax.cn/anthropic/v1/messages").url(),
            "https://api.minimax.cn/anthropic/v1/messages"
        );
    }

    #[test]
    fn a_profile_starts_at_the_current_version_and_says_what_it_changed() {
        let profile = Profile::base("https://x");
        assert_eq!(profile.api_version, API_VERSION);
        assert_eq!(profile.auth, Auth::None);
        assert!(profile.betas.is_empty());
        let profile = profile
            .with_bearer_token("t")
            .with_version("2024-01-01")
            .with_beta("a")
            .with_beta("b");
        assert_eq!(profile.auth, Auth::Bearer("t".into()));
        assert_eq!(profile.api_version, "2024-01-01");
        assert_eq!(profile.betas, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn an_endpoint_that_cannot_be_a_url_is_a_configuration_error() {
        let err = Client::new(Profile::endpoint("ht tp://x")).unwrap_err();
        assert!(matches!(err, Error::Config(_)), "{err:?}");
        assert!(!err.is_transient());
        let err = Client::new(Profile::endpoint("file:///tmp/x")).unwrap_err();
        assert!(err.to_string().contains("http or https"), "{err}");
    }

    #[tokio::test]
    async fn the_standard_headers_and_the_body_go_out_together() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("authorization", "Bearer sk-test"))
            .and(header("anthropic-version", API_VERSION))
            .and(header("content-type", "application/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "msg_1",
                "model": "m",
                "content": [{"type": "text", "text": "hello"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 3, "output_tokens": 2},
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = client_for(&server, Profile::endpoint("x").with_bearer_token("sk-test"));
        let message = client
            .messages(&request())
            .await
            .expect("the answer parses");
        assert_eq!(message.id, "msg_1");
        assert_eq!(message.content, vec![crate::types::Block::text("hello")]);
        assert_eq!(message.usage.input_tokens, Some(3));
    }

    #[tokio::test]
    async fn the_legacy_key_header_is_available_for_an_endpoint_that_wants_it() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(header("x-api-key", "k"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "m", "content": [], "usage": {}
            })))
            .mount(&server)
            .await;
        let client = client_for(&server, Profile::endpoint("x").with_api_key("k"));
        assert!(client.messages(&request()).await.is_ok());
        // And nothing claims to be authenticated by the other form.
        let sent = server.received_requests().await.unwrap();
        assert!(sent[0].headers.get("authorization").is_none());
    }

    #[tokio::test]
    async fn a_profile_with_no_credential_sends_no_auth_header() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "m"})))
            .mount(&server)
            .await;
        let client = client_for(&server, Profile::endpoint("x"));
        assert!(client.messages(&request()).await.is_ok());
        let sent = server.received_requests().await.unwrap();
        assert!(sent[0].headers.get("authorization").is_none());
        assert!(sent[0].headers.get("x-api-key").is_none());
    }

    #[tokio::test]
    async fn beta_names_ride_in_one_comma_separated_header() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(headers(
                "anthropic-beta",
                vec![
                    "interleaved-thinking-2025-05-14",
                    "fine-grained-tool-streaming-2025-05-14",
                ],
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "m"})))
            .mount(&server)
            .await;
        let client = client_for(
            &server,
            Profile::endpoint("x")
                .with_beta("interleaved-thinking-2025-05-14")
                .with_beta("fine-grained-tool-streaming-2025-05-14"),
        );
        let outcome = client.messages(&request()).await;
        assert!(outcome.is_ok(), "outcome: {outcome:?}");
        // One header, names in the order they were added: the wire's own form.
        let sent = server.received_requests().await.unwrap();
        assert_eq!(
            sent[0].headers.get("anthropic-beta").unwrap(),
            "interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14"
        );
    }

    #[tokio::test]
    async fn a_refusal_keeps_the_status_and_the_endpoints_own_words() {
        let server = MockServer::start().await;
        let body = json!({"type": "error", "error": {"type": "invalid_request_error",
            "message": "max_tokens: must be greater than or equal to 1"}});
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(400).set_body_json(&body))
            .mount(&server)
            .await;
        let client = client_for(&server, Profile::endpoint("x"));
        let err = client.messages(&request()).await.unwrap_err();
        assert!(
            !err.is_transient(),
            "a 400 will be refused the same way twice"
        );
        let shown = err.to_string();
        assert!(shown.contains("400"), "{shown}");
        assert!(shown.contains("max_tokens"), "{shown}");
    }

    #[tokio::test]
    async fn an_overloaded_endpoint_is_a_failure_worth_retrying() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(529).set_body_string("overloaded"))
            .mount(&server)
            .await;
        let client = client_for(&server, Profile::endpoint("x"));
        let err = client.stream(&request()).await.unwrap_err();
        assert!(err.is_transient());
    }

    #[tokio::test]
    async fn streaming_hands_back_the_events_of_the_answer() {
        let server = MockServer::start().await;
        let body = [
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"usage\":{\"input_tokens\":9}}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ]
        .concat();
        Mock::given(method("POST"))
            .and(header("accept", "text/event-stream"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
            .mount(&server)
            .await;

        let client = client_for(&server, Profile::endpoint("x"));
        let mut stream = client.stream(&request()).await.expect("the stream opens");
        let mut usage = Usage::default();
        let mut text = String::new();
        while let Some(event) = stream.next_event().await.expect("the events read") {
            match event {
                crate::types::Event::MessageStart { message } => usage.merge(&message.usage),
                crate::types::Event::ContentBlockDelta {
                    delta: crate::types::BlockDelta::TextDelta { text: fragment },
                    ..
                } => text.push_str(&fragment),
                crate::types::Event::MessageDelta { usage: Some(u), .. } => usage.merge(&u),
                _ => {}
            }
        }
        assert_eq!(text, "hi");
        assert_eq!(
            usage.input_tokens,
            Some(9),
            "the prompt came from the start"
        );
        assert_eq!(usage.output_tokens, Some(2), "the answer from the end");
        assert_eq!(usage.total_tokens(), 11);
    }
}
