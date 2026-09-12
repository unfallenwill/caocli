//! The client: where the endpoint is, how to authenticate, and the four calls
//! the API has.
//!
//! Everything an endpoint may differ in is a field of [`Profile`] and nothing
//! else in this crate is: the path, which header carries the credential. A
//! backend that speaks the spec is a `Profile`, not a branch.

use std::future::Future;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::{Api, Error};
use crate::responses_stream::ResponseEventStream;
use crate::responses_types::{Response, ResponseCreateRequest};
use crate::stream::ChunkStream;
use crate::types::{ChatCompletion, ChatCompletionRequest};

/// The path the Chat Completions API answers at, under a base URL whose
/// `/v1/` is dropped to land here.
const CHAT_COMPLETIONS_PATH: &str = "/chat/completions";

/// The path the Responses API answers at.
const RESPONSES_PATH: &str = "/responses";

/// How long a connection may take to open. The reference client's own bound.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a stream may be silent before it is a failure.
///
/// A read timeout, not a total one: it resets after every byte that arrives, so
/// a long answer is never cut off for being long, and a connection that has
/// stopped delivering is given up on. The reference client bounds the same
/// thing at ten minutes, and nothing bounds it here.
const READ_TIMEOUT: Duration = Duration::from_secs(600);

/// What this crate calls itself to the endpoint. The reference client sends
/// its own name and version the same way, and a request that arrives
/// anonymously is a request nobody can account for.
const USER_AGENT: &str = concat!("caocli-openai/", env!("CARGO_PKG_VERSION"));

/// How many attempts a request gets before it is a failure: the first one and
/// this many more. The reference client's own default.
pub const DEFAULT_MAX_RETRIES: u32 = 2;

/// The first wait of a backoff, and what it doubles from.
const INITIAL_RETRY_DELAY: Duration = Duration::from_millis(500);

/// The longest wait a backoff reaches, however many attempts have been made.
const MAX_RETRY_DELAY: Duration = Duration::from_secs(8);

/// The longest wait an endpoint's own `retry-after` can ask for and be obeyed.
/// Past this the backoff is used instead: an endpoint that wants a minute or
/// an hour is an endpoint to come back to later, not to hold a turn open for.
/// The reference client's own bound.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(120);

/// How the client proves who it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Auth {
    /// `Authorization: Bearer <token>`: the primary form.
    Bearer(String),
    /// Nothing. For an endpoint that authenticates some other way — one
    /// behind a proxy, or one that asks for no key at all.
    None,
}

/// Where the completions go, and who is sending them.
///
/// Built rather than assembled. The base URL is the API root the reference
/// client's `base_url` is — the path `/v1/chat/completions` is appended by
/// [`Client::completion`] and [`Client::stream_completion`], and
/// `/v1/responses` by [`Client::responses`] and [`Client::stream_responses`].
/// A backend that puts its resource paths somewhere else names them with
/// [`Profile::endpoint`] and the methods still append the standard tail.
///
/// A caller that wants to point one client at both Chat Completions and
/// Responses names the base as `https://api.openai.com/v1` (or whatever its
/// OpenAI-compatible gateway's `/v1` is) and uses the methods as they are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    base: String,
    auth: Auth,
    organization: Option<String>,
    project: Option<String>,
    max_retries: u32,
}

impl Profile {
    /// A profile for the given base URL. The base is the API root: `/v1` for
    /// the standard OpenAI endpoint, or a gateway's equivalent. Resource paths
    /// (`/chat/completions`, `/responses`) are appended by the methods that
    /// call them.
    pub fn base(base: impl Into<String>) -> Self {
        Self {
            base: base.into(),
            auth: Auth::None,
            organization: None,
            project: None,
            max_retries: DEFAULT_MAX_RETRIES,
        }
    }

    /// A profile for an endpoint named in full, path and all. The base is
    /// stored verbatim; methods append `/chat/completions` or `/responses` to
    /// it. For an OpenAI-compatible gateway that hosts both resources at a
    /// custom path, name the path up to where the standard resource paths
    /// begin.
    pub fn endpoint(endpoint: impl Into<String>) -> Self {
        Self {
            base: endpoint.into(),
            auth: Auth::None,
            organization: None,
            project: None,
            max_retries: DEFAULT_MAX_RETRIES,
        }
    }

    /// Authenticate with `Authorization: Bearer`, the primary form.
    pub fn with_bearer_token(mut self, token: impl Into<String>) -> Self {
        self.auth = Auth::Bearer(token.into());
        self
    }

    /// Name the organization the request runs on behalf of, in the
    /// `OpenAI-Organization` header. Required by a key that belongs to more
    /// than one organization, and ignored by the others.
    pub fn with_organization(mut self, organization: impl Into<String>) -> Self {
        self.organization = Some(organization.into());
        self
    }

    /// Name the project the request runs in, in the `OpenAI-Project` header.
    pub fn with_project(mut self, project: impl Into<String>) -> Self {
        self.project = Some(project.into());
        self
    }

    /// Send to a different endpoint instead, everything else unchanged — a
    /// proxy in front of the API, or the endpoint of a test.
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.base = endpoint.into();
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

    /// The base URL methods on Client append their resource paths to.
    pub fn url(&self) -> &str {
        &self.base
    }

    /// The base URL the Chat Completions path is appended to.
    fn chat_completions_url(&self) -> String {
        let trimmed = self.base.trim_end_matches('/');
        // A base that already ends in `/chat/completions` is taken as-is, the
        // way `Profile::endpoint("https://.../chat/completions")` says.
        if trimmed.ends_with(CHAT_COMPLETIONS_PATH) {
            trimmed.to_string()
        } else {
            // Drop a trailing `/v1` if present, then append the resource path
            // under `/v1/`. A base that already includes `/v1/...` is left
            // alone.
            let without_v1 = trimmed
                .strip_suffix("/v1")
                .or_else(|| trimmed.strip_suffix("/v1/"))
                .unwrap_or(trimmed);
            format!("{without_v1}/v1{CHAT_COMPLETIONS_PATH}")
        }
    }

    /// The base URL the Responses path is appended to.
    fn responses_url(&self) -> String {
        let trimmed = self.base.trim_end_matches('/');
        // A base that already names the responses endpoint is taken as-is.
        if trimmed.ends_with(RESPONSES_PATH) {
            return trimmed.to_string();
        }
        // A base that names the chat completions endpoint gets `/chat/completions`
        // swapped for `/responses`, so a caller that names the full
        // Chat Completions URL still gets the right Responses URL.
        if let Some(stripped) = trimmed.strip_suffix(CHAT_COMPLETIONS_PATH) {
            return format!("{stripped}{RESPONSES_PATH}");
        }
        // A base that ends in `/v1` is left alone, the resource path is
        // appended under it.
        let without_v1 = trimmed
            .strip_suffix("/v1")
            .or_else(|| trimmed.strip_suffix("/v1/"))
            .unwrap_or(trimmed);
        format!("{without_v1}/v1{RESPONSES_PATH}")
    }

    /// Test-only: the URL the Chat Completions methods append their path to.
    #[cfg(test)]
    fn chat_completions_url_for_test(&self) -> String {
        self.chat_completions_url()
    }

    /// Test-only: the URL the Responses methods append their path to.
    #[cfg(test)]
    fn responses_url_for_test(&self) -> String {
        self.responses_url()
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
            .field("base", &self.profile.base)
            .field(
                "auth",
                &match &self.profile.auth {
                    Auth::Bearer(_) => "Bearer(…)",
                    Auth::None => "None",
                },
            )
            .field("organization", &self.profile.organization)
            .field("project", &self.profile.project)
            .field("max_retries", &self.profile.max_retries)
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
        let parsed = reqwest::Url::parse(&profile.base).map_err(|e| {
            Error::Config(format!(
                "endpoint URL does not parse: {:?} ({e})",
                profile.base
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
    /// [`Client::stream_completion`] is the one to prefer for a long answer:
    /// streaming is the only way a large answer survives the read timeout, and
    /// an answer that is displayed as it is written is the point of the call.
    /// This call is the non-streaming one: it forces `stream: false` on the
    /// body and returns the whole answer at once.
    pub async fn completion(
        &self,
        request: &ChatCompletionRequest,
    ) -> Result<ChatCompletion, Error> {
        let mut asked = request.clone();
        asked.stream = false;
        let body = serde_json::to_vec(&asked).map_err(|source| Error::Decode {
            what: "request",
            source,
        })?;
        let endpoint = self.profile.chat_completions_url();
        self.answer("message", || self.post(&endpoint, body.clone()))
            .await
    }

    /// An answer as it is written, one chunk at a time.
    ///
    /// The body asks for a stream whether or not the request did: `stream: true`
    /// is a field the endpoint reads, and a caller who reached for *this* call
    /// has already asked for one. The reference client's own streaming helper
    /// puts it in the body for the same reason — its non-streaming path takes
    /// whatever the caller said, and this call is the one that means "stream".
    pub async fn stream_completion(
        &self,
        request: &ChatCompletionRequest,
    ) -> Result<ChunkStream, Error> {
        let mut asked = request.clone();
        asked.stream = true;
        let body = serde_json::to_vec(&asked).map_err(|source| Error::Decode {
            what: "request",
            source,
        })?;
        let endpoint = self.profile.chat_completions_url();
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
            Ok(ChunkStream::new(Box::pin(response.bytes_stream())))
        })
        .await
    }

    /// One complete answer to a Responses request, delivered at once.
    ///
    /// [`Client::stream_responses`] is the one to prefer for a long answer:
    /// the Responses wire is event-based, and the streaming surface is where
    /// the model's tool calls and reasoning arrive as they happen. This call
    /// is the non-streaming one: it forces `stream: false` on the body and
    /// returns the whole answer at once.
    pub async fn responses(&self, request: &ResponseCreateRequest) -> Result<Response, Error> {
        let mut asked = request.clone();
        asked.stream = false;
        let body = serde_json::to_vec(&asked).map_err(|source| Error::Decode {
            what: "request",
            source,
        })?;
        let endpoint = self.profile.responses_url();
        self.answer("response", || self.post(&endpoint, body.clone()))
            .await
    }

    /// A Responses answer as it is written, one event at a time.
    ///
    /// The body asks for a stream whether or not the request did: `stream: true`
    /// is a field the endpoint reads, and a caller who reached for *this* call
    /// has already asked for one. Events arrive as the wire writes them, with
    /// `data: [DONE]` closing the stream.
    pub async fn stream_responses(
        &self,
        request: &ResponseCreateRequest,
    ) -> Result<ResponseEventStream, Error> {
        let mut asked = request.clone();
        asked.stream = true;
        let body = serde_json::to_vec(&asked).map_err(|source| Error::Decode {
            what: "request",
            source,
        })?;
        let endpoint = self.profile.responses_url();
        self.retrying(|| async {
            let response = self
                .post(&endpoint, body.clone())
                .send()
                .await
                .map_err(Error::Transport)?;
            let status = response.status();
            if !status.is_success() {
                let headers = response.headers().clone();
                let body = response.text().await.unwrap_or_default();
                return Err(Error::Api(refusal(status.as_u16(), &headers, body)));
            }
            Ok(ResponseEventStream::new(Box::pin(response.bytes_stream())))
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
    /// Only the establishing of a response is retried. A stream that fails
    /// after its first chunk is not: the answer is half-delivered and the
    /// caller is the one that can decide what to do about it.
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
                    // An infinity literal on `retry-after` is the endpoint
                    // saying "wait forever" — refuse the retry rather than
                    // sleep for `Duration::MAX`. The reference client does
                    // the same by treating it as "exceeds max retry-after".
                    if let Some(asked) = failure.retry_after()
                        && asked == Duration::MAX
                    {
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
            .header("accept", "application/json")
            .header("content-type", "application/json")
            .header("user-agent", USER_AGENT);
        request = match &self.profile.auth {
            Auth::Bearer(token) => request.bearer_auth(token),
            Auth::None => request,
        };
        if let Some(organization) = &self.profile.organization {
            request = request.header("OpenAI-Organization", organization);
        }
        if let Some(project) = &self.profile.project {
            request = request.header("OpenAI-Project", project);
        }
        request
    }

    /// A POST of a body, to the profile's endpoint.
    fn post(&self, url: &str, body: Vec<u8>) -> reqwest::RequestBuilder {
        self.request(reqwest::Method::POST, url).body(body)
    }

    /// Send a request and read the JSON answer, retrying what is worth retrying.
    ///
    /// One place for the shape every call has: build, send, read what the
    /// headers said before the body takes the response, and decode. `what`
    /// names the payload in a decode failure, because a refusal that cannot
    /// be parsed and a body that cannot be parsed are worth telling apart in
    /// a log.
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
}

/// Parse an endpoint's `Retry-After` value, in any of the three forms the
/// HTTP spec (and the reference client) accept:
///
/// - A number of seconds, possibly fractional: `120`, `1.5`.
/// - The literal `inf`, `+inf`, `infinity`, `+infinity`, which the reference
///   client reads as "wait forever — refuse to retry".
/// - An HTTP-date (`Wed, 21 Oct 2015 07:28:00 GMT`), the third form RFC 7231
///   allows.
///
/// `None` is returned when the value is missing, malformed, or the date is in
/// the past. `Some(Duration::ZERO)` is returned only for the zero-seconds case,
/// which the schedule handles by retrying at once.
///
/// **This parser treats the value as seconds.** The nonstandard
/// `retry-after-ms` header carries milliseconds; the caller divides by 1000
/// before calling.
fn parse_retry_after(value: &str) -> Option<Duration> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    // The four infinity literals the reference client treats as "wait forever".
    let lower = trimmed.to_ascii_lowercase();
    if matches!(lower.as_str(), "inf" | "+inf" | "infinity" | "+infinity") {
        return Some(Duration::MAX);
    }
    if let Ok(delay) = trimmed.parse::<f64>() {
        if delay.is_finite() {
            return Some(Duration::from_secs_f64(delay.max(0.0)));
        }
        // Numeric overflow is treated as "wait forever" too — an absurdly
        // large number is not the same as a literal `inf`, but both refuse
        // a retry within the schedule's window.
        return Some(Duration::MAX);
    }
    // Last: HTTP-date. We can't bring in `email` or `chrono` just for this,
    // so parse the three common RFC 7231 / RFC 1123 / asctime shapes by hand.
    parse_http_date(trimmed)
}

/// Parse the nonstandard `retry-after-ms` header: a number of milliseconds,
/// possibly fractional. Infinity literals are honored as "wait forever".
fn parse_retry_after_ms(value: &str) -> Option<Duration> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    if matches!(lower.as_str(), "inf" | "+inf" | "infinity" | "+infinity") {
        return Some(Duration::MAX);
    }
    if let Ok(ms) = trimmed.parse::<f64>() {
        if ms.is_finite() {
            return Some(Duration::from_secs_f64((ms / 1000.0).max(0.0)));
        }
        return Some(Duration::MAX);
    }
    None
}

/// Parse an HTTP-date and return the seconds from now until then. Returns
/// `None` when the value does not look like one of the three common date
/// shapes, or when the date is in the past (a past date is treated as
/// "retry immediately").
///
/// The shapes this recognizes:
/// - RFC 1123: `Wed, 21 Oct 2015 07:28:00 GMT`
/// - RFC 850:  `Wednesday, 21-Oct-15 07:28:00 GMT` (year is two digits)
/// - asctime:  `Wed Oct 21 07:28:00 2015`
fn parse_http_date(value: &str) -> Option<Duration> {
    use std::time::{SystemTime, UNIX_EPOCH};

    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;

    // Try the three shapes in order. Each returns a (year, month, day, h, m, s)
    // tuple or `None`.
    let parsed = parse_rfc1123(value)
        .or_else(|| parse_rfc850(value))
        .or_else(|| parse_asctime(value))?;

    let (y, mo, d, h, mi, s) = parsed;

    // Days from Unix epoch to the given date, in the Gregorian calendar.
    let days = days_from_epoch(y, mo, d)?;
    let target = days
        .checked_mul(86_400)?
        .checked_add((h as i64).checked_mul(3600)?)?
        .checked_add((mi as i64).checked_mul(60)?)?
        .checked_add(s as i64)?;
    let delta = target - now;
    if delta <= 0 {
        return Some(Duration::ZERO);
    }
    Some(Duration::from_secs(delta as u64))
}

fn parse_rfc1123(value: &str) -> Option<(i32, u32, u32, u32, u32, u32)> {
    // `Sun, 06 Nov 1994 08:49:37 GMT`
    let mut parts = value.split_whitespace();
    let _dow = parts.next()?;
    let day: u32 = parts.next()?.parse().ok()?;
    let mon = match parts.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: i32 = parts.next()?.parse().ok()?;
    let hms = parts.next()?;
    let mut t = hms.split(':');
    let h: u32 = t.next()?.parse().ok()?;
    let mi: u32 = t.next()?.parse().ok()?;
    let s: u32 = t.next()?.parse().ok()?;
    let tz = parts.next()?;
    if !matches!(tz, "GMT" | "UT" | "Z") {
        return None;
    }
    Some((year, mon, day, h, mi, s))
}

fn parse_rfc850(value: &str) -> Option<(i32, u32, u32, u32, u32, u32)> {
    // `Wednesday, 21-Oct-15 07:28:00 GMT`
    let mut parts = value.split_whitespace();
    let _dow = parts.next()?;
    let date = parts.next()?;
    let mut d = date.split('-');
    let day: u32 = d.next()?.parse().ok()?;
    let mon = match d.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let yy: i32 = d.next()?.parse().ok()?;
    let year = if yy < 70 { 2000 + yy } else { 1900 + yy };
    let hms = parts.next()?;
    let mut t = hms.split(':');
    let h: u32 = t.next()?.parse().ok()?;
    let mi: u32 = t.next()?.parse().ok()?;
    let s: u32 = t.next()?.parse().ok()?;
    let tz = parts.next()?;
    if !matches!(tz, "GMT" | "UT" | "Z") {
        return None;
    }
    Some((year, mon, day, h, mi, s))
}

fn parse_asctime(value: &str) -> Option<(i32, u32, u32, u32, u32, u32)> {
    // `Wed Oct 21 07:28:00 2015`
    let mut parts = value.split_whitespace();
    let _dow = parts.next()?;
    let mon = match parts.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let day: u32 = parts.next()?.parse().ok()?;
    let hms = parts.next()?;
    let mut t = hms.split(':');
    let h: u32 = t.next()?.parse().ok()?;
    let mi: u32 = t.next()?.parse().ok()?;
    let s: u32 = t.next()?.parse().ok()?;
    let year: i32 = parts.next()?.parse().ok()?;
    Some((year, mon, day, h, mi, s))
}

/// Days from the Unix epoch (1970-01-01) to the given date, in proleptic
/// Gregorian. Uses Howard Hinnant's `days_from_civil` algorithm.
fn days_from_epoch(y: i32, m: u32, d: u32) -> Option<i64> {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u32; // [0, 399]
    let m = m as i32;
    let d = d as i32;
    let doy = ((153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1) as u32; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    Some((era as i64) * 146097 + (doe as i64) - 719468)
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
        request_id: text("x-request-id"),
        should_retry: match text("x-should-retry").as_deref() {
            Some("true") => Some(true),
            Some("false") => Some(false),
            _ => None,
        },
        // Milliseconds first (nonstandard, faster), then seconds (per RFC),
        // then an HTTP-date as a last resort. The reference client reads them
        // in this order too: the more precise one wins when both are set.
        retry_after: text("retry-after-ms")
            .as_deref()
            .and_then(parse_retry_after_ms)
            .or_else(|| text("retry-after").as_deref().and_then(parse_retry_after)),
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
    use crate::responses_types::ResponseCreateRequest;
    use crate::types::{ChatCompletionChunk, ChatCompletionMessageParam};
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn request() -> ChatCompletionRequest {
        ChatCompletionRequest::streaming("gpt-4o", vec![ChatCompletionMessageParam::user("hi")])
    }

    /// A client for a mock endpoint, which is where every test here sends.
    fn client_for(server: &MockServer, profile: Profile) -> Client {
        Client::new(profile.with_endpoint(server.uri() + "/v1/chat/completions"))
            .expect("the mock URL parses")
    }

    /// A client for a mock Responses endpoint.
    fn client_for_responses(server: &MockServer, profile: Profile) -> Client {
        Client::new(profile.with_endpoint(server.uri() + "/v1/responses"))
            .expect("the mock URL parses")
    }

    #[test]
    fn a_base_url_is_stored_as_is_and_resource_paths_are_appended() {
        // The base is the API root; resource paths are appended by the methods
        // that call them.
        assert_eq!(
            Profile::base("https://api.openai.com").url(),
            "https://api.openai.com"
        );
        assert_eq!(
            Profile::base("https://api.openai.com/").url(),
            "https://api.openai.com/"
        );
        // A base that names the `/v1` segment is left alone, and the methods
        // append the resource path under it.
        assert_eq!(
            Profile::base("https://api.openai.com/v1").url(),
            "https://api.openai.com/v1"
        );
        // A gateway that puts the path somewhere else names it in full.
        assert_eq!(
            Profile::endpoint("https://api.deepseek.com/v1/chat/completions").url(),
            "https://api.deepseek.com/v1/chat/completions"
        );
    }

    #[test]
    fn chat_completions_and_responses_urls_are_built_from_the_base() {
        // A plain base gets `/v1/...` appended to it.
        let p = Profile::base("https://api.openai.com");
        assert_eq!(
            p.chat_completions_url_for_test(),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            p.responses_url_for_test(),
            "https://api.openai.com/v1/responses"
        );

        // A base that already ends in `/v1` is left alone.
        let p = Profile::base("https://api.openai.com/v1");
        assert_eq!(
            p.chat_completions_url_for_test(),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            p.responses_url_for_test(),
            "https://api.openai.com/v1/responses"
        );

        // A full chat completions URL: kept as-is for chat, dropped to its
        // base for responses.
        let p = Profile::endpoint("https://api.deepseek.com/v1/chat/completions");
        assert_eq!(
            p.chat_completions_url_for_test(),
            "https://api.deepseek.com/v1/chat/completions"
        );
        assert_eq!(
            p.responses_url_for_test(),
            "https://api.deepseek.com/v1/responses"
        );
    }

    #[test]
    fn a_profile_starts_with_no_credential_and_says_what_it_changed() {
        let profile = Profile::base("https://x");
        assert_eq!(profile.auth, Auth::None);
        assert_eq!(profile.max_retries, DEFAULT_MAX_RETRIES);
        let profile = profile
            .with_bearer_token("t")
            .with_organization("org_01")
            .with_project("proj_01")
            .with_max_retries(5);
        assert_eq!(profile.auth, Auth::Bearer("t".into()));
        assert_eq!(profile.organization.as_deref(), Some("org_01"));
        assert_eq!(profile.project.as_deref(), Some("proj_01"));
        assert_eq!(profile.max_retries, 5);
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
    async fn the_organization_and_project_headers_go_out_when_a_caller_names_them() {
        // A key that belongs to more than one organization has to say which,
        // and a project-scoped request has to say which project.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(header("OpenAI-Organization", "org_01"))
            .and(header("OpenAI-Project", "proj_01"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "cmpl-1",
                "choices": [{
                    "finish_reason": "stop",
                    "index": 0,
                    "message": {"role": "assistant", "content": "ok"}
                }],
                "created": 1,
                "model": "gpt-4o",
                "object": "chat.completion"
            })))
            .mount(&server)
            .await;
        let client = client_for(
            &server,
            Profile::endpoint("x")
                .with_bearer_token("t")
                .with_organization("org_01")
                .with_project("proj_01"),
        );
        assert!(client.completion(&request().non_streaming()).await.is_ok());

        // And a profile that names neither sends neither.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "cmpl-1",
                "choices": [{
                    "finish_reason": "stop",
                    "index": 0,
                    "message": {"role": "assistant", "content": "ok"}
                }],
                "created": 1,
                "model": "gpt-4o",
                "object": "chat.completion"
            })))
            .mount(&server)
            .await;
        let client = client_for(&server, Profile::endpoint("x"));
        assert!(client.completion(&request().non_streaming()).await.is_ok());
        let sent = server.received_requests().await.unwrap();
        assert!(sent[0].headers.get("OpenAI-Organization").is_none());
        assert!(sent[0].headers.get("OpenAI-Project").is_none());
    }

    #[tokio::test]
    async fn the_standard_headers_and_the_body_go_out_together() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(header("authorization", "Bearer sk-test"))
            .and(header("user-agent", USER_AGENT))
            .and(header("accept", "application/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "cmpl-1",
                "choices": [{
                    "finish_reason": "stop",
                    "index": 0,
                    "message": {"role": "assistant", "content": "ok"}
                }],
                "created": 1,
                "model": "gpt-4o",
                "object": "chat.completion"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let client = client_for(&server, Profile::endpoint("x").with_bearer_token("sk-test"));
        let completion = client.completion(&request().non_streaming()).await.unwrap();
        assert_eq!(completion.choices[0].message.content.as_deref(), Some("ok"));
    }

    #[tokio::test]
    async fn a_non_streaming_call_forces_stream_false() {
        // The non-streaming call does not respect a `stream: true` left in the
        // request; the body it sends has stream: false.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "cmpl-1",
                "choices": [{
                    "finish_reason": "stop",
                    "index": 0,
                    "message": {"role": "assistant", "content": "ok"}
                }],
                "created": 1,
                "model": "gpt-4o",
                "object": "chat.completion"
            })))
            .mount(&server)
            .await;
        let client = client_for(&server, Profile::endpoint("x").with_bearer_token("sk-test"));
        // The request says stream: true; the call still sends stream: false.
        assert!(client.completion(&request()).await.is_ok());
        let sent = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&sent[0].body).unwrap();
        assert_eq!(body["stream"], json!(false));
    }

    #[tokio::test]
    async fn a_streaming_call_yields_each_chunk_in_order() {
        let server = MockServer::start().await;
        let body = [
            "data: {\"id\":\"cmpl-1\",\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"index\":0,\"finish_reason\":null}],\"created\":1,\"model\":\"gpt-4o\",\"object\":\"chat.completion.chunk\"}\n\n",
            "data: {\"id\":\"cmpl-1\",\"choices\":[{\"delta\":{\"content\":\"hi\"},\"index\":0,\"finish_reason\":null}],\"created\":1,\"model\":\"gpt-4o\",\"object\":\"chat.completion.chunk\"}\n\n",
            "data: {\"id\":\"cmpl-1\",\"choices\":[{\"delta\":{},\"index\":0,\"finish_reason\":\"stop\"}],\"created\":1,\"model\":\"gpt-4o\",\"object\":\"chat.completion.chunk\"}\n\n",
            "data: [DONE]\n\n",
        ].concat();
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
        let client = client_for(&server, Profile::endpoint("x").with_bearer_token("sk-test"));
        let mut stream = client.stream_completion(&request()).await.unwrap();
        let mut seen = Vec::new();
        while let Some(chunk) = stream.next_chunk().await.unwrap() {
            seen.push(chunk);
        }
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[0].choices[0].delta.content.as_deref(), Some(""));
        assert_eq!(seen[1].choices[0].delta.content.as_deref(), Some("hi"));
        assert_eq!(
            seen[2].choices[0].finish_reason.as_ref().unwrap().as_str(),
            "stop"
        );
    }

    #[tokio::test]
    async fn a_request_that_fails_429_retries_with_a_backoff() {
        let server = MockServer::start().await;
        // Two 429s, then a 200. With max_retries = 2 that is one attempt and
        // two retries — the third request gets through.
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
            .up_to_n_times(2)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "cmpl-1",
                "choices": [{
                    "finish_reason": "stop",
                    "index": 0,
                    "message": {"role": "assistant", "content": "ok"}
                }],
                "created": 1,
                "model": "gpt-4o",
                "object": "chat.completion"
            })))
            .mount(&server)
            .await;
        let client = client_for(
            &server,
            Profile::endpoint("x")
                .with_bearer_token("sk-test")
                .with_max_retries(2),
        );
        let completion = client.completion(&request().non_streaming()).await.unwrap();
        assert_eq!(completion.choices[0].message.content.as_deref(), Some("ok"));
        let sent = server.received_requests().await.unwrap();
        assert_eq!(sent.len(), 3, "the first plus two retries");
    }

    #[tokio::test]
    async fn a_request_that_fails_400_does_not_retry() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
            .mount(&server)
            .await;
        let client = client_for(&server, Profile::endpoint("x").with_bearer_token("sk-test"));
        let err = client
            .completion(&request().non_streaming())
            .await
            .unwrap_err();
        assert!(!err.is_transient(), "400 is not worth retrying");
        let sent = server.received_requests().await.unwrap();
        assert_eq!(sent.len(), 1, "no retries on a permanent refusal");
    }

    #[tokio::test]
    async fn retry_after_ms_wins_over_retry_after_when_both_are_set() {
        // The reference client's order: `retry-after-ms` first, in milliseconds,
        // because the endpoint sending both meant the more precise one.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after-ms", "100")
                    .insert_header("retry-after", "10")
                    .set_body_string("rate limited"),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "cmpl-1",
                "choices": [{
                    "finish_reason": "stop",
                    "index": 0,
                    "message": {"role": "assistant", "content": "ok"}
                }],
                "created": 1,
                "model": "gpt-4o",
                "object": "chat.completion"
            })))
            .mount(&server)
            .await;
        let client = client_for(
            &server,
            Profile::endpoint("x")
                .with_bearer_token("sk-test")
                .with_max_retries(1),
        );
        let started = std::time::Instant::now();
        let completion = client.completion(&request().non_streaming()).await.unwrap();
        assert_eq!(completion.choices[0].message.content.as_deref(), Some("ok"));
        // retry-after-ms = 100ms, plus a little scheduling slack.
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn a_request_id_is_read_and_carried_in_the_api_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(500)
                    .insert_header("x-request-id", "req_01")
                    .set_body_string("server error"),
            )
            .mount(&server)
            .await;
        let client = client_for(&server, Profile::endpoint("x").with_bearer_token("sk-test"));
        let err = client
            .completion(&request().non_streaming())
            .await
            .unwrap_err();
        assert_eq!(err.request_id(), Some("req_01"));
    }

    #[tokio::test]
    async fn debug_redacts_the_bearer_token() {
        let client = Client::new(
            Profile::endpoint("https://api.openai.com/v1/chat/completions")
                .with_bearer_token("sk-secret"),
        )
        .unwrap();
        let shown = format!("{client:?}");
        assert!(shown.contains("Bearer(…)"), "{shown}");
        assert!(!shown.contains("sk-secret"), "{shown}");
    }

    #[tokio::test]
    async fn an_unknown_chunk_field_is_read_past_and_the_stream_continues() {
        // The wire is tolerant: an endpoint that adds a chunk field does not
        // fail a stream that is still serving one.
        let server = MockServer::start().await;
        let body = [
            "data: {\"id\":\"cmpl-1\",\"choices\":[{\"delta\":{\"content\":\"hi\"},\"index\":0,\"finish_reason\":null}],\"created\":1,\"model\":\"gpt-4o\",\"object\":\"chat.completion.chunk\",\"future_field\":{\"x\":1}}\n\n",
            "data: [DONE]\n\n",
        ].concat();
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
        let client = client_for(&server, Profile::endpoint("x").with_bearer_token("sk-test"));
        let mut stream = client.stream_completion(&request()).await.unwrap();
        let chunk: ChatCompletionChunk = stream.next_chunk().await.unwrap().unwrap();
        assert_eq!(chunk.choices[0].delta.content.as_deref(), Some("hi"));
        assert!(stream.next_chunk().await.unwrap().is_none());
    }

    fn responses_request() -> ResponseCreateRequest {
        ResponseCreateRequest::streaming("gpt-4o", "hi")
    }

    #[tokio::test]
    async fn a_responses_call_posts_to_v1_responses_and_decodes_the_answer() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "resp_1",
                "created_at": 1.0,
                "model": "gpt-4o",
                "object": "response",
                "output": [{
                    "id": "msg_1",
                    "type": "message",
                    "role": "assistant",
                    "status": "completed",
                    "content": [{"type": "output_text", "text": "hi"}]
                }],
                "parallel_tool_calls": true,
                "tool_choice": "auto",
                "tools": [],
                "status": "completed",
            })))
            .mount(&server)
            .await;
        let client =
            client_for_responses(&server, Profile::endpoint("x").with_bearer_token("sk-test"));
        let response = client.responses(&responses_request()).await.unwrap();
        assert_eq!(response.id, "resp_1");
        assert_eq!(response.model, "gpt-4o");
        assert_eq!(response.output.len(), 1);
        let sent = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&sent[0].body).unwrap();
        assert_eq!(body["stream"], json!(false));
        assert_eq!(body["model"], json!("gpt-4o"));
    }

    #[tokio::test]
    async fn a_streaming_responses_call_yields_each_event_in_order() {
        let server = MockServer::start().await;
        let body = [
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"created_at\":1.0,\"status\":\"in_progress\"},\"sequence_number\":0}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0,\"delta\":\"hi\",\"logprobs\":[],\"sequence_number\":1}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"created_at\":1.0,\"status\":\"completed\"},\"sequence_number\":2}\n\n",
            "data: [DONE]\n\n",
        ].concat();
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
        let client =
            client_for_responses(&server, Profile::endpoint("x").with_bearer_token("sk-test"));
        let mut stream = client.stream_responses(&responses_request()).await.unwrap();
        let mut seen = Vec::new();
        while let Some(event) = stream.next_event().await.unwrap() {
            seen.push(event);
        }
        assert_eq!(seen.len(), 3);
        assert!(matches!(
            seen[0],
            crate::ResponseStreamEvent::Created { .. }
        ));
        if let crate::ResponseStreamEvent::OutputTextDelta { delta, .. } = &seen[1] {
            assert_eq!(delta, "hi");
        } else {
            panic!("expected output text delta, got {:?}", seen[1]);
        }
        assert!(matches!(
            seen[2],
            crate::ResponseStreamEvent::Completed { .. }
        ));
        let sent = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&sent[0].body).unwrap();
        assert_eq!(body["stream"], json!(true));
    }

    #[tokio::test]
    async fn a_responses_call_that_fails_429_retries_with_a_backoff() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
            .up_to_n_times(2)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "resp_1",
                "created_at": 1.0,
                "model": "gpt-4o",
                "object": "response",
                "output": [],
                "parallel_tool_calls": true,
                "tool_choice": "auto",
                "tools": [],
                "status": "completed",
            })))
            .mount(&server)
            .await;
        let client = client_for_responses(
            &server,
            Profile::endpoint("x")
                .with_bearer_token("sk-test")
                .with_max_retries(2),
        );
        let response = client.responses(&responses_request()).await.unwrap();
        assert_eq!(response.id, "resp_1");
        let sent = server.received_requests().await.unwrap();
        assert_eq!(sent.len(), 3, "the first plus two retries");
    }

    #[tokio::test]
    async fn one_client_can_call_both_surfaces() {
        // A Profile that names the base: the methods append their own paths,
        // so the same Client can call Chat Completions and Responses.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "cmpl-1",
                "choices": [{
                    "finish_reason": "stop",
                    "index": 0,
                    "message": {"role": "assistant", "content": "ok"}
                }],
                "created": 1,
                "model": "gpt-4o",
                "object": "chat.completion"
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "resp_1",
                "created_at": 1.0,
                "model": "gpt-4o",
                "object": "response",
                "output": [],
                "parallel_tool_calls": true,
                "tool_choice": "auto",
                "tools": [],
                "status": "completed",
            })))
            .mount(&server)
            .await;
        let client = Client::new(Profile::base(server.uri() + "/v1").with_bearer_token("sk-test"))
            .expect("the mock URL parses");

        let chat = client.completion(&request().non_streaming()).await.unwrap();
        assert_eq!(chat.id, "cmpl-1");
        let responses = client.responses(&responses_request()).await.unwrap();
        assert_eq!(responses.id, "resp_1");
    }

    // ─────── Retry-After parsing ───────

    #[test]
    fn retry_after_numeric_seconds_are_parsed() {
        assert_eq!(parse_retry_after("120"), Some(Duration::from_secs(120)));
        assert_eq!(parse_retry_after("1.5"), Some(Duration::from_millis(1500)));
        assert_eq!(parse_retry_after("0"), Some(Duration::ZERO));
        assert_eq!(parse_retry_after("-1"), Some(Duration::ZERO));
    }

    #[test]
    fn retry_after_inf_literals_refuse_a_retry() {
        // The reference client treats all four infinity literals as "wait
        // forever — refuse the retry". My crate returns Duration::MAX so the
        // retry loop gives up rather than sleeping for the heat death of
        // the universe.
        for lit in ["inf", "+inf", "infinity", "+infinity", "INF", " Infinity "] {
            assert_eq!(parse_retry_after(lit), Some(Duration::MAX));
        }
    }

    #[test]
    fn retry_after_malformed_or_past_returns_none_or_zero() {
        assert_eq!(parse_retry_after(""), None);
        assert_eq!(parse_retry_after("not-a-date"), None);
        // A date in the past: the call retries immediately, not later.
        let past = "Wed, 01 Jan 2020 00:00:00 GMT";
        assert_eq!(parse_retry_after(past), Some(Duration::ZERO));
    }

    #[test]
    fn retry_after_ms_divides_by_1000() {
        // The nonstandard milliseconds header is divided by 1000 by the
        // caller; the parser only knows about seconds.
        assert_eq!(
            parse_retry_after_ms("500"),
            Some(Duration::from_millis(500))
        );
        assert_eq!(
            parse_retry_after_ms("1500.0"),
            Some(Duration::from_millis(1500))
        );
        assert_eq!(parse_retry_after_ms("inf"), Some(Duration::MAX));
        assert_eq!(parse_retry_after_ms("not-a-number"), None);
    }

    #[test]
    fn retry_after_http_dates_are_parsed_in_all_three_shapes() {
        // The HTTP-date the reference client reads as a last resort. We
        // can't pin an exact duration (the test would race the clock), so
        // we just check that a future date parses as something finite, and
        // that the three shapes are all recognized.
        let in_two_hours = now_plus(2 * 3600);
        let rfc1123 = format_http1123(in_two_hours);
        assert!(
            parse_retry_after(&rfc1123).unwrap() > Duration::from_secs(3600),
            "{rfc1123}"
        );
        let rfc850 = format_http850(in_two_hours);
        assert!(
            parse_retry_after(&rfc850).unwrap() > Duration::from_secs(3600),
            "{rfc850}"
        );
        let asctime = format_asctime(in_two_hours);
        assert!(
            parse_retry_after(&asctime).unwrap() > Duration::from_secs(3600),
            "{asctime}"
        );
    }

    /// Seconds since the Unix epoch, computed the same way `parse_http_date`
    /// computes them: the date at midnight UTC of the given offset.
    fn now_plus(seconds: i64) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            + seconds
    }

    fn format_http1123(unix_secs: i64) -> String {
        // Walk the days from epoch to build an RFC 1123 date.
        let (y, m, d, hh, mm, ss) = civil_from_unix(unix_secs);
        let dow = dow_from_unix(unix_secs);
        format!(
            "{dow}, {d:02} {mon} {y:04} {hh:02}:{mm:02}:{ss:02} GMT",
            mon = month_name(m)
        )
    }

    fn format_http850(unix_secs: i64) -> String {
        let (y, m, d, hh, mm, ss) = civil_from_unix(unix_secs);
        let dow = dow_full(unix_secs);
        format!(
            "{dow}, {d:02}-{mon}-{yy:02} {hh:02}:{mm:02}:{ss:02} GMT",
            mon = month_name(m),
            yy = y % 100
        )
    }

    fn format_asctime(unix_secs: i64) -> String {
        let (y, m, d, hh, mm, ss) = civil_from_unix(unix_secs);
        let dow = dow_short(unix_secs);
        format!(
            "{dow} {mon} {d:02} {hh:02}:{mm:02}:{ss:02} {y:04}",
            mon = month_name(m)
        )
    }

    fn civil_from_unix(unix_secs: i64) -> (i32, u32, u32, u32, u32, u32) {
        let days = unix_secs.div_euclid(86_400);
        let secs_of_day = unix_secs.rem_euclid(86_400) as u32;
        let hh = secs_of_day / 3600;
        let mm = (secs_of_day % 3600) / 60;
        let ss = secs_of_day % 60;
        // Inverse of days_from_epoch.
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = (z - era * 146_097) as u32;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
        let y = (yoe as i32) + ((era * 400) as i32);
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let m_raw = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = if m_raw <= 2 { y + 1 } else { y };
        (y, m_raw, doy - (153 * mp + 2) / 5 + 1, hh, mm, ss)
    }

    fn dow_from_unix(unix_secs: i64) -> &'static str {
        ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"]
            [(unix_secs.div_euclid(86_400).rem_euclid(7)) as usize]
    }

    fn dow_short(unix_secs: i64) -> &'static str {
        ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"]
            [(unix_secs.div_euclid(86_400).rem_euclid(7)) as usize]
    }

    fn dow_full(unix_secs: i64) -> &'static str {
        [
            "Sunday",
            "Monday",
            "Tuesday",
            "Wednesday",
            "Thursday",
            "Friday",
            "Saturday",
        ][(unix_secs.div_euclid(86_400).rem_euclid(7)) as usize]
    }

    fn month_name(m: u32) -> &'static str {
        match m {
            1 => "Jan",
            2 => "Feb",
            3 => "Mar",
            4 => "Apr",
            5 => "May",
            6 => "Jun",
            7 => "Aug",
            8 => "Sep",
            9 => "Oct",
            10 => "Oct",
            11 => "Nov",
            _ => "Dec",
        }
    }
}
