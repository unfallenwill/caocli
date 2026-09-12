//! The client: where the endpoint is, how to authenticate, and the two calls
//! the API has.
//!
//! Everything an endpoint may differ in is a field of [`Profile`] and nothing
//! else in this crate is: the path, the version header, which header carries the
//! credential, the beta names. A backend that speaks the spec is a `Profile`,
//! not a branch.

use std::future::Future;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::{Api, Error};
use crate::stream::EventStream;
use crate::types::{
    BatchPage, BatchRequest, BatchResult, CountTokensRequest, CountTokensResponse,
    DeletedMessageBatch, Message, MessageBatch, MessagesRequest,
};

/// The path the Messages API answers at, under a base URL.
const MESSAGES_PATH: &str = "/v1/messages";

/// The path a token count answers at, hung off the messages path: the count is
/// about a prompt, and the prompt is what the messages path describes.
const COUNT_TOKENS_PATH: &str = "/count_tokens";

/// The path batches answer at, hung off the messages path for the same reason:
/// a batch is many of those requests.
const BATCHES_PATH: &str = "/batches";

/// The API version this crate speaks. Requests name it — the header is
/// required, and a request without one is refused rather than defaulted.
pub const API_VERSION: &str = "2023-06-01";

/// How long a connection may take to open. The reference client's own bound.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a stream may be silent before it is a failure.
///
/// A read timeout, not a total one: it resets after every byte that arrives, so
/// a long answer is never cut off for being long, and a connection that has
/// stopped delivering is given up on. The reference client bounds the same
/// thing at ten minutes, and nothing bounds it here.
const READ_TIMEOUT: Duration = Duration::from_secs(600);

/// What this crate calls itself to the endpoint. The reference client sends its
/// own name and version the same way, and a request that arrives anonymously is
/// a request nobody can account for.
const USER_AGENT: &str = concat!("caocli-anthropic/", env!("CARGO_PKG_VERSION"));

/// How many attempts a request gets before it is a failure: the first one and
/// this many more. The reference client's own default.
pub const DEFAULT_MAX_RETRIES: u32 = 2;

/// The first wait of a backoff, and what it doubles from.
const INITIAL_RETRY_DELAY: Duration = Duration::from_millis(500);

/// The longest wait a backoff reaches, however many attempts have been made.
const MAX_RETRY_DELAY: Duration = Duration::from_secs(8);

/// The longest wait an endpoint's own `retry-after` can ask for and be obeyed.
/// Past this the backoff is used instead: an endpoint that wants a minute or an
/// hour is an endpoint to come back to later, not to hold a turn open for.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

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
    max_retries: u32,
    workspace_id: Option<String>,
    user_profile_id: Option<String>,
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
            max_retries: DEFAULT_MAX_RETRIES,
            workspace_id: None,
            user_profile_id: None,
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

    /// Name the workspace the request runs in, in the `anthropic-workspace-id`
    /// header. Required by a key that belongs to more than one workspace, and
    /// ignored by the others.
    pub fn with_workspace_id(mut self, workspace_id: impl Into<String>) -> Self {
        self.workspace_id = Some(workspace_id.into());
        self
    }

    /// Name the user profile the request is made on behalf of, in the
    /// `anthropic-user-profile-id` header.
    pub fn with_user_profile_id(mut self, user_profile_id: impl Into<String>) -> Self {
        self.user_profile_id = Some(user_profile_id.into());
        self
    }

    /// How many times a request worth retrying is tried again. The default is
    /// [`DEFAULT_MAX_RETRIES`], which is the reference client's own.
    ///
    /// `0` means an attempt is the whole of it: the first failure is the caller's
    /// failure. A caller that would rather see a refusal than wait through a
    /// backoff — an interactive one, where the wait shows up as a turn that has
    /// stopped responding — says so here.
    pub fn with_max_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = max_retries;
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
            .connect_timeout(CONNECT_TIMEOUT)
            // Read, not total: an answer is allowed to take as long as it
            // takes, and only silence is a failure. A total timeout would cut
            // off a long answer that is arriving perfectly well.
            .read_timeout(READ_TIMEOUT)
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
        let endpoint = self.profile.endpoint.clone();
        self.answer("message", || self.post(&endpoint, body.clone()))
            .await
    }

    /// An answer as it is written, one event at a time.
    ///
    /// The body asks for a stream whether or not the request did: `stream: true`
    /// is a field the endpoint reads, and a caller who reached for *this* call has
    /// already asked for one. The reference client's own streaming helper puts it
    /// in the body for the same reason — its non-streaming path takes whatever
    /// the caller said, and this call is the one that means "stream".
    ///
    /// No `accept` header of its own goes with it: the `stream` field in the
    /// body is what asks for a stream. The usual `text/event-stream` is what a
    /// reader of the SSE specification would reach for, and neither the
    /// published examples nor the reference client send it — the transport's
    /// own `*/*` rides along and nothing more, which is exactly what the
    /// reference client's transport sends.
    pub async fn stream(&self, request: &MessagesRequest) -> Result<EventStream, Error> {
        let mut asked = request.clone();
        asked.stream = Some(true);
        let body = self.body(&asked)?;
        let endpoint = self.profile.endpoint.clone();
        self.retrying(|| async {
            let response = self
                .post(&endpoint, body.clone())
                .send()
                .await
                .map_err(Error::Transport)?;
            let status = response.status();
            if !status.is_success() {
                // What the headers said is read before the body takes the
                // response: whether to try again is in them.
                let headers = response.headers().clone();
                let body = response.text().await.unwrap_or_default();
                return Err(Error::Api(refusal(status.as_u16(), &headers, body)));
            }
            Ok(EventStream::new(Box::pin(response.bytes_stream())))
        })
        .await
    }

    /// How many tokens a prompt is, by the endpoint's own count.
    ///
    /// The count is the tokenizer's answer, not an estimate: useful before
    /// sending something large, and the only way to see how much of a context
    /// window a conversation is holding.
    pub async fn count_tokens(&self, request: &MessagesRequest) -> Result<u64, Error> {
        let body = self.body(&CountTokensRequest::of(request))?;
        let url = self.path(COUNT_TOKENS_PATH);
        let answer: CountTokensResponse = self
            .answer("token count", || self.post(&url, body.clone()))
            .await?;
        Ok(answer.input_tokens)
    }

    /// Send many requests at once and be told about them later.
    ///
    /// A batch is the out-of-band way to ask: the endpoint answers each request
    /// on its own time, and [`Client::batch_results`] collects them. Useful for
    /// work nobody is waiting for — not for a turn, which is a conversation.
    pub async fn batch_create(&self, requests: Vec<BatchRequest>) -> Result<MessageBatch, Error> {
        let body =
            serde_json::to_vec(&serde_json::json!({ "requests": requests })).map_err(|source| {
                Error::Decode {
                    what: "batch request",
                    source,
                }
            })?;
        let url = self.path(BATCHES_PATH);
        self.answer("batch", || self.post(&url, body.clone())).await
    }

    /// How one batch is doing.
    pub async fn batch_retrieve(&self, batch_id: &str) -> Result<MessageBatch, Error> {
        let url = self.path(&format!("{BATCHES_PATH}/{batch_id}"));
        self.answer("batch", || self.request(reqwest::Method::GET, &url))
            .await
    }

    /// A page of batches, the newest first unless a caller says otherwise.
    ///
    /// One page: `after_id` asks for the page that follows the one whose last id
    /// this is, and the page says whether there is more.
    pub async fn batch_list(
        &self,
        after_id: Option<&str>,
        limit: Option<u32>,
    ) -> Result<BatchPage, Error> {
        let mut url = self.path(BATCHES_PATH);
        let mut query: Vec<String> = Vec::new();
        if let Some(after_id) = after_id {
            query.push(format!("after_id={after_id}"));
        }
        if let Some(limit) = limit {
            query.push(format!("limit={limit}"));
        }
        if !query.is_empty() {
            url = format!("{url}?{}", query.join("&"));
        }
        self.answer("batch page", || self.request(reqwest::Method::GET, &url))
            .await
    }

    /// A batch's answers, once it has ended.
    ///
    /// Served as JSON Lines rather than as JSON: one result per line, in no
    /// particular order, which is why this returns them as a list rather than
    /// preserving anything about how they arrived.
    pub async fn batch_results(&self, batch_id: &str) -> Result<Vec<BatchResult>, Error> {
        let url = self.path(&format!("{BATCHES_PATH}/{batch_id}/results"));
        let text = self
            .retrying(|| async {
                let response = self
                    .request(reqwest::Method::GET, &url)
                    .send()
                    .await
                    .map_err(Error::Transport)?;
                let status = response.status();
                let headers = response.headers().clone();
                if !status.is_success() {
                    let body = response.text().await.unwrap_or_default();
                    return Err(Error::Api(refusal(status.as_u16(), &headers, body)));
                }
                response.text().await.map_err(Error::Transport)
            })
            .await?;
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                serde_json::from_str(line).map_err(|source| Error::Decode {
                    what: "batch result",
                    source,
                })
            })
            .collect()
    }

    /// Ask for a batch to stop. Requests already answered keep their answers.
    pub async fn batch_cancel(&self, batch_id: &str) -> Result<MessageBatch, Error> {
        let url = self.path(&format!("{BATCHES_PATH}/{batch_id}/cancel"));
        self.answer("batch", || self.post(&url, Vec::new())).await
    }

    /// Throw a batch away. Its answers go with it.
    pub async fn batch_delete(&self, batch_id: &str) -> Result<DeletedMessageBatch, Error> {
        let url = self.path(&format!("{BATCHES_PATH}/{batch_id}"));
        self.answer("deleted batch", || {
            self.request(reqwest::Method::DELETE, &url)
        })
        .await
    }

    /// Run one attempt, and another one while the failure is worth another one.
    ///
    /// The schedule is the reference client's: the endpoint's own `retry-after`
    /// when it asked for a wait a client should obey, and otherwise a backoff
    /// that doubles from half a second to eight, spread by a jitter so that a
    /// fleet of clients does not come back in lockstep.
    ///
    /// Only the establishing of a response is retried. A stream that fails after
    /// its first event is not: the answer is half-delivered and the caller is the
    /// one that can decide what to do about it.
    async fn retrying<T, F, Fut>(&self, attempt: F) -> Result<T, Error>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<T, Error>>,
    {
        let mut made = 0;
        loop {
            match attempt().await {
                Ok(value) => return Ok(value),
                Err(failure) => {
                    if made >= self.profile.max_retries || !failure.is_transient() {
                        return Err(failure);
                    }
                    let delay = retry_delay(made, failure.retry_after());
                    made += 1;
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }

    /// A request with everything this profile puts on every one of them.
    fn request(&self, method: reqwest::Method, url: &str) -> reqwest::RequestBuilder {
        let mut request = self
            .http
            .request(method, url)
            .header("user-agent", USER_AGENT)
            .header("content-type", "application/json")
            .header("anthropic-version", &self.profile.api_version);
        request = match &self.profile.auth {
            Auth::Bearer(token) => request.bearer_auth(token),
            Auth::ApiKey(key) => request.header("x-api-key", key),
            Auth::None => request,
        };
        if !self.profile.betas.is_empty() {
            request = request.header("anthropic-beta", self.profile.betas.join(","));
        }
        if let Some(workspace_id) = &self.profile.workspace_id {
            request = request.header("anthropic-workspace-id", workspace_id);
        }
        if let Some(user_profile_id) = &self.profile.user_profile_id {
            request = request.header("anthropic-user-profile-id", user_profile_id);
        }
        request
    }

    /// A POST of a body, to a path of the endpoint.
    fn post(&self, url: &str, body: Vec<u8>) -> reqwest::RequestBuilder {
        self.request(reqwest::Method::POST, url).body(body)
    }

    /// Send a request and read the JSON answer, retrying what is worth retrying.
    ///
    /// One place for the shape every call has: build, send, read what the
    /// headers said before the body takes the response, and decode. `what` names
    /// the payload in a decode failure, because a batch that cannot be read and a
    /// message that cannot be read are worth telling apart in a log.
    async fn answer<T, F>(&self, what: &'static str, attempt: F) -> Result<T, Error>
    where
        T: serde::de::DeserializeOwned,
        F: Fn() -> reqwest::RequestBuilder,
    {
        self.retrying(|| async {
            let response = attempt().send().await.map_err(Error::Transport)?;
            let status = response.status();
            let headers = response.headers().clone();
            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                return Err(Error::Api(refusal(status.as_u16(), &headers, body)));
            }
            let bytes = response.bytes().await.map_err(Error::Transport)?;
            serde_json::from_slice(&bytes).map_err(|source| Error::Decode { what, source })
        })
        .await
    }

    /// The path a sub-resource of the messages endpoint lives at.
    fn path(&self, tail: &str) -> String {
        format!("{}{tail}", self.profile.endpoint)
    }

    /// A request's body, as the endpoint reads it. Generic over the request type
    /// because a count is a request of its own shape.
    fn body<T: serde::Serialize>(&self, request: &T) -> Result<Vec<u8>, Error> {
        serde_json::to_vec(request).map_err(|source| Error::Decode {
            what: "request",
            source,
        })
    }
}

/// A refusal, with what its headers said about it.
fn refusal(status: u16, headers: &reqwest::header::HeaderMap, body: String) -> Api {
    let text = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    Api {
        status,
        body,
        request_id: text("request-id"),
        should_retry: match text("x-should-retry").as_deref() {
            Some("true") => Some(true),
            Some("false") => Some(false),
            _ => None,
        },
        // Milliseconds first, as the reference client reads them: an endpoint
        // that sends both means the more precise one. What was asked for is kept
        // as it was; whether a client should obey it is the schedule's business.
        retry_after: text("retry-after-ms")
            .and_then(|ms| ms.parse::<f64>().ok())
            .map(|ms| Duration::from_secs_f64((ms / 1000.0).max(0.0)))
            .or_else(|| {
                text("retry-after")
                    .and_then(|s| s.parse::<f64>().ok())
                    .map(|s| Duration::from_secs_f64(s.max(0.0)))
            }),
    }
}

/// How long to wait before attempt number `made + 1`.
fn retry_delay(made: u32, retry_after: Option<Duration>) -> Duration {
    // The endpoint's own answer first, when it is one a client should obey: a
    // wait it asked for that is longer than this is a wait to come back after,
    // not one to hold a turn open for.
    if let Some(asked) = retry_after
        && asked > Duration::ZERO
        && asked <= MAX_RETRY_AFTER
    {
        return asked;
    }
    let backoff = INITIAL_RETRY_DELAY
        .saturating_mul(2u32.saturating_pow(made))
        .min(MAX_RETRY_DELAY);
    backoff.mul_f64(jitter())
}

/// The fraction of a backoff to actually wait: between three quarters and all of
/// it, which is the reference client's own spread (`1 - 0.25 * random()`).
///
/// The clock's low bits are randomness enough for a jitter whose whole job is to
/// keep two clients from retrying in the same millisecond, and it costs the crate
/// no dependency.
fn jitter() -> f64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.subsec_nanos())
        .unwrap_or_default();
    1.0 - 0.25 * (f64::from(nanos % 1_000_000) / 1_000_000.0)
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
    async fn the_workspace_and_profile_headers_go_out_when_a_caller_names_them() {
        // A key that belongs to more than one workspace has to say which, and a
        // request made on somebody's behalf has to say whose.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(header("anthropic-workspace-id", "wrkspc_01"))
            .and(header("anthropic-user-profile-id", "prof_01"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "m"})))
            .mount(&server)
            .await;
        let client = client_for(
            &server,
            Profile::endpoint("x")
                .with_workspace_id("wrkspc_01")
                .with_user_profile_id("prof_01"),
        );
        assert!(client.messages(&request()).await.is_ok());

        // And a profile that names neither sends neither.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "m"})))
            .mount(&server)
            .await;
        let client = client_for(&server, Profile::endpoint("x"));
        assert!(client.messages(&request()).await.is_ok());
        let sent = server.received_requests().await.unwrap();
        assert!(sent[0].headers.get("anthropic-workspace-id").is_none());
        assert!(sent[0].headers.get("anthropic-user-profile-id").is_none());
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
    async fn the_client_names_itself_and_asks_for_nothing_extra() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "m"})))
            .mount(&server)
            .await;
        let client = client_for(&server, Profile::endpoint("x"));
        assert!(client.messages(&request()).await.is_ok());
        let sent = server.received_requests().await.unwrap();
        assert_eq!(
            sent[0].headers.get("user-agent").unwrap(),
            USER_AGENT,
            "a request that arrives anonymously is one nobody can account for"
        );
        // The body's `stream` field is what asks for a stream. The transport's
        // own default rides along — reqwest sends `*/*` here exactly as httpx
        // does for the reference client — and the SSE spelling is not sent,
        // because neither baseline sends it.
        assert_eq!(sent[0].headers.get("accept").unwrap(), "*/*");
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
    async fn a_batch_is_made_read_and_collected_on_its_own_paths() {
        let server = MockServer::start().await;
        let batch = json!({
            "id": "msgbatch_1", "type": "message_batch", "processing_status": "in_progress",
            "request_counts": {"processing": 1, "succeeded": 0, "errored": 0,
                               "canceled": 0, "expired": 0},
            "created_at": "2026-01-01T00:00:00Z", "expires_at": "2026-01-02T00:00:00Z",
            "ended_at": null, "cancel_initiated_at": null, "results_url": null,
        });
        Mock::given(method("POST"))
            .and(path("/v1/messages/batches"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&batch))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/messages/batches/msgbatch_1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&batch))
            .mount(&server)
            .await;
        // A page of them, which is what a list answers with.
        Mock::given(method("GET"))
            .and(path("/v1/messages/batches"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [batch], "has_more": false,
                "first_id": "msgbatch_1", "last_id": "msgbatch_1",
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/messages/batches/msgbatch_1/cancel"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&batch))
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/v1/messages/batches/msgbatch_1"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"id": "msgbatch_1", "type": "message_batch_deleted"})),
            )
            .mount(&server)
            .await;
        // The results are JSON Lines, one result per line.
        Mock::given(method("GET"))
            .and(path("/v1/messages/batches/msgbatch_1/results"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                concat!(
                    r#"{"custom_id":"a","result":{"type":"succeeded","message":{"id":"msg_1","content":[{"type":"text","text":"four"}]}}}"#,
                    "\n",
                    r#"{"custom_id":"b","result":{"type":"errored","error":{"type":"invalid_request_error","message":"nope"}}}"#,
                    "\n",
                ),
            ))
            .mount(&server)
            .await;

        let client = client_for(&server, Profile::endpoint("x"));
        let made = client
            .batch_create(vec![BatchRequest {
                custom_id: "a".into(),
                params: request(),
            }])
            .await
            .expect("the batch is made");
        assert_eq!(made.id, "msgbatch_1");
        assert_eq!(
            made.processing_status,
            Some(crate::types::BatchStatus::InProgress)
        );
        assert_eq!(made.request_counts.unwrap().processing, 1);

        let fetched = client.batch_retrieve("msgbatch_1").await.unwrap();
        assert_eq!(fetched.id, made.id);
        assert_eq!(
            client
                .batch_list(Some("msgbatch_1"), Some(1))
                .await
                .unwrap()
                .data
                .len(),
            1
        );
        assert_eq!(client.batch_cancel("msgbatch_1").await.unwrap().id, made.id);
        assert_eq!(client.batch_delete("msgbatch_1").await.unwrap().id, made.id);

        let results = client.batch_results("msgbatch_1").await.unwrap();
        assert_eq!(results.len(), 2, "one per line, and the blank one ignored");
        assert_eq!(results[0].custom_id, "a");
        match &results[0].result {
            crate::types::BatchOutcome::Succeeded { message } => {
                assert_eq!(message.content, vec![crate::types::Block::text("four")]);
            }
            other => panic!("expected an answer, got {other:?}"),
        }
        assert!(matches!(
            results[1].result,
            crate::types::BatchOutcome::Errored { .. }
        ));

        // The list is asked with its paging in the query, where the endpoint
        // reads it.
        let sent = server.received_requests().await.unwrap();
        let listed = sent
            .iter()
            .find(|request| {
                request.url.path() == "/v1/messages/batches"
                    && request.method == reqwest::Method::GET
            })
            .expect("the list was asked for");
        assert!(
            listed.url.query().unwrap().contains("after_id=msgbatch_1"),
            "{listed:?}"
        );
        assert!(
            listed.url.query().unwrap().contains("limit=1"),
            "{listed:?}"
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
        // No retries here: the point is what the failure *is*, and the retries of
        // a 529 are the next test's subject.
        let client = client_for(&server, Profile::endpoint("x").with_max_retries(0));
        let err = client.stream(&request()).await.unwrap_err();
        assert!(err.is_transient());
        assert_eq!(err.kind(), crate::error::Kind::Overloaded);
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    /// The retry schedule, which is the reference client's: it is a *behavior*,
    /// so the tests are about how many attempts the endpoint saw and how long the
    /// client waited between them.

    #[tokio::test]
    async fn a_refusal_worth_retrying_is_tried_again_and_the_answer_arrives() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429).set_body_string("slow down"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "m"})))
            .mount(&server)
            .await;
        let client = client_for(&server, Profile::endpoint("x"));
        assert!(client.messages(&request()).await.is_ok());
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            2,
            "one refusal, one answer"
        );
    }

    #[tokio::test]
    async fn a_wait_the_endpoint_asked_for_is_the_wait_that_happens() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after-ms", "5")
                    .set_body_string("slow down"),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "m"})))
            .mount(&server)
            .await;
        let client = client_for(&server, Profile::endpoint("x"));
        let started = std::time::Instant::now();
        assert!(client.messages(&request()).await.is_ok());
        // A backoff would have waited at least 375 ms; five milliseconds is what
        // the endpoint asked for, and what it gets.
        assert!(
            started.elapsed() < Duration::from_millis(300),
            "waited {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn a_refusal_that_will_not_change_is_not_tried_again() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
            .mount(&server)
            .await;
        let client = client_for(&server, Profile::endpoint("x"));
        let err = client.messages(&request()).await.unwrap_err();
        assert_eq!(err.kind(), crate::error::Kind::BadRequest);
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn what_the_endpoint_says_about_retrying_is_believed() {
        // A status this client retries, and the endpoint saying not to.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("x-should-retry", "false")
                    .set_body_string("no"),
            )
            .mount(&server)
            .await;
        let client = client_for(&server, Profile::endpoint("x"));
        assert!(client.messages(&request()).await.is_err());
        assert_eq!(server.received_requests().await.unwrap().len(), 1);

        // A status it does not retry, and the endpoint saying to.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(400)
                    .insert_header("x-should-retry", "true")
                    .set_body_string("try again"),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "m"})))
            .mount(&server)
            .await;
        let client = client_for(&server, Profile::endpoint("x"));
        assert!(client.messages(&request()).await.is_ok());
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_caller_that_wants_no_retries_gets_none() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(503).set_body_string("later"))
            .mount(&server)
            .await;
        let client = client_for(&server, Profile::endpoint("x").with_max_retries(0));
        assert!(client.messages(&request()).await.is_err());
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "the first failure is the caller's failure"
        );
        // And the budget is the reference client's unless a caller says
        // otherwise.
        assert_eq!(Profile::endpoint("x").max_retries, DEFAULT_MAX_RETRIES);
        assert_eq!(DEFAULT_MAX_RETRIES, 2);
    }

    #[tokio::test]
    async fn a_refusal_carries_the_endpoints_own_id_for_the_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(500)
                    .insert_header("request-id", "req_01")
                    .set_body_string("boom"),
            )
            .mount(&server)
            .await;
        let client = client_for(&server, Profile::endpoint("x").with_max_retries(0));
        let err = client.messages(&request()).await.unwrap_err();
        assert_eq!(err.request_id(), Some("req_01"));
        assert_eq!(err.kind(), crate::error::Kind::ServerError);
    }

    #[tokio::test]
    async fn a_request_that_never_arrived_is_tried_again() {
        // Nothing listens on port 1: every attempt is a connection failure, which
        // is one of the failures the reference client retries.
        let client = Client::new(Profile::endpoint("http://127.0.0.1:1/never")).unwrap();
        let started = std::time::Instant::now();
        let err = client.messages(&request()).await.unwrap_err();
        assert!(
            matches!(
                err.kind(),
                crate::error::Kind::Connection | crate::error::Kind::Timeout
            ),
            "{err:?}"
        );
        // Three attempts, two waits: the shortest they can be is 375 ms and
        // 750 ms, and the bound is loose because the jitter is real.
        assert!(
            started.elapsed() >= Duration::from_secs(1),
            "waited {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn streaming_asks_the_endpoint_for_a_stream_whatever_the_request_said() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw("data: {\"type\":\"message_stop\"}\n\n", "text/event-stream"),
            )
            .mount(&server)
            .await;
        let client = client_for(&server, Profile::endpoint("x"));

        // A request that never mentioned streaming.
        let plain = MessagesRequest::new("m", 16, vec![MessageParam::user("hi")]);
        assert!(client.stream(&plain).await.is_ok());
        // And one that said not to.
        let mut declined = plain.clone();
        declined.stream = Some(false);
        assert!(client.stream(&declined).await.is_ok());

        for request in server.received_requests().await.unwrap() {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(body["stream"], true, "the body is what asks for a stream");
        }
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
