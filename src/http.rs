// Copied by wuapi-codegen from packages/sdk-codegen/templates/rust/src/http.rs. Do not edit here.

//! The HTTP runtime: client configuration, timeouts, retries and idempotency
//! keys.
//!
//! Every attempt has a timeout. A request is sent again only when that cannot
//! repeat its effect: `GET`, `PUT` and `DELETE` requests, requests that carry
//! an idempotency key the API accepts, requests that never reached the server
//! (the connection could not be opened) and requests the server refused with
//! `429`. Operations that accept an idempotency key get a random one when the
//! caller gives none, and every attempt sends the same key.

use std::fmt;
use std::future::{Future, IntoFuture};
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::Error;
use crate::meta::{API_KEY_ENV, DEFAULT_BASE_URL, IDEMPOTENCY_HEADER, PROJECT_HEADER, USER_AGENT};

/// Response header with the request id.
const REQUEST_ID_HEADER: &str = "x-request-id";
/// Longest accepted project header value.
const MAX_PROJECT_LEN: usize = 200;
/// Default per-attempt timeout.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// When and how often a failed request is sent again.
///
/// Waits grow from `initial_backoff`, doubling per attempt up to
/// `max_backoff`, with jitter (between half the wait and the whole wait). A
/// `Retry-After` header replaces the computed wait; one longer than
/// `max_retry_after` (a cooldown) fails at once instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Retries after the first attempt. Defaults to 2. `0` turns retries off.
    pub max_retries: u32,
    /// The wait before the first retry. Defaults to 500 ms.
    pub initial_backoff: Duration,
    /// The longest computed wait. Defaults to 8 s.
    pub max_backoff: Duration,
    /// The longest `Retry-After` the client waits for. Defaults to 60 s.
    pub max_retry_after: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 2,
            initial_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(8),
            max_retry_after: Duration::from_secs(60),
        }
    }
}

impl RetryPolicy {
    /// A policy that never retries.
    #[must_use]
    pub fn none() -> Self {
        Self {
            max_retries: 0,
            ..Self::default()
        }
    }

    /// The wait before retry number `attempt` (0 for the first retry).
    fn backoff(&self, attempt: u32) -> Duration {
        let factor = 2u32.saturating_pow(attempt.min(16));
        let base = self
            .initial_backoff
            .saturating_mul(factor)
            .min(self.max_backoff);
        let half = base / 2;
        half + half.mul_f64(random_unit())
    }
}

/// A random number in `[0, 1)`.
fn random_unit() -> f64 {
    // 53 random bits: every value is exactly representable as an f64.
    let bits = (uuid::Uuid::new_v4().as_u128() >> 75) as u64;
    bits as f64 / (1u64 << 53) as f64
}

/// A random idempotency key, so automatic retries never repeat a request.
#[must_use]
pub fn random_key() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Configures and builds a client.
#[derive(Clone, Default)]
pub struct ClientBuilder {
    api_key: Option<String>,
    base_url: Option<String>,
    timeout: Option<Duration>,
    retry: Option<RetryPolicy>,
    project: Option<String>,
    http_client: Option<reqwest::Client>,
}

impl fmt::Debug for ClientBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientBuilder")
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("base_url", &self.base_url)
            .field("timeout", &self.timeout)
            .field("retry", &self.retry)
            .field("project", &self.project)
            .finish_non_exhaustive()
    }
}

impl ClientBuilder {
    /// A builder with every default.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The API key. Without it the client reads the API key environment
    /// variable ([`API_KEY_ENV`]).
    #[must_use]
    pub fn api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }

    /// The API base URL. Defaults to the production URL
    /// ([`DEFAULT_BASE_URL`]).
    #[must_use]
    pub fn base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = Some(base_url.into());
        self
    }

    /// The timeout of each attempt, from connecting until the response body
    /// has been read. Defaults to 30 s.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// The retry policy. Defaults to [`RetryPolicy::default`].
    #[must_use]
    pub fn retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = Some(retry);
        self
    }

    /// Retries after the first attempt, keeping the rest of the policy.
    #[must_use]
    pub fn max_retries(mut self, max_retries: u32) -> Self {
        self.retry
            .get_or_insert_with(RetryPolicy::default)
            .max_retries = max_retries;
        self
    }

    /// Act inside one project: its id, or `ext:<externalId>`. Sent as the
    /// project header on every request. With a project API key it may be
    /// omitted (the key already names its project).
    #[must_use]
    pub fn project(mut self, project: impl Into<String>) -> Self {
        self.project = Some(project.into());
        self
    }

    /// A `reqwest` client to send requests with, to bring your own proxy,
    /// TLS or connection pool settings. The per-attempt timeout and the
    /// `User-Agent` of this crate still apply.
    #[must_use]
    pub fn http_client(mut self, client: reqwest::Client) -> Self {
        self.http_client = Some(client);
        self
    }

    /// Builds the client.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when there is no API key, the project is longer than
    /// 200 characters, or the HTTP client cannot be created.
    pub fn build(self) -> Result<crate::Client, Error> {
        self.build_http().map(crate::Client::from)
    }

    /// Builds the low-level HTTP client.
    ///
    /// # Errors
    ///
    /// The same as [`ClientBuilder::build`].
    pub fn build_http(self) -> Result<HttpClient, Error> {
        let api_key = self
            .api_key
            .or_else(|| std::env::var(API_KEY_ENV).ok())
            .filter(|key| !key.trim().is_empty())
            .ok_or_else(|| {
                Error::Config(format!(
                    "missing API key: pass one or set the {API_KEY_ENV} environment variable"
                ))
            })?;
        let base_url = self
            .base_url
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_owned())
            .trim_end_matches('/')
            .to_owned();
        let project = normalize_project(self.project.as_deref())?;
        let client = match self.http_client {
            Some(client) => client,
            None => reqwest::Client::builder()
                .build()
                .map_err(|e| Error::Config(format!("cannot create the HTTP client: {e}")))?,
        };
        Ok(HttpClient {
            inner: Arc::new(Inner {
                api_key,
                base_url,
                timeout: self.timeout.unwrap_or(DEFAULT_TIMEOUT),
                retry: self.retry.unwrap_or_default(),
                project,
                client,
            }),
        })
    }
}

fn normalize_project(project: Option<&str>) -> Result<Option<String>, Error> {
    let Some(project) = project.map(str::trim).filter(|p| !p.is_empty()) else {
        return Ok(None);
    };
    if project.chars().count() > MAX_PROJECT_LEN {
        return Err(Error::Config(format!(
            "`project` must be at most {MAX_PROJECT_LEN} characters"
        )));
    }
    Ok(Some(project.to_owned()))
}

struct Inner {
    api_key: String,
    base_url: String,
    timeout: Duration,
    retry: RetryPolicy,
    project: Option<String>,
    client: reqwest::Client,
}

/// The low-level client every resource shares. Cheap to clone.
#[derive(Clone)]
pub struct HttpClient {
    inner: Arc<Inner>,
}

impl fmt::Debug for HttpClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpClient")
            .field("base_url", &self.inner.base_url)
            .field("timeout", &self.inner.timeout)
            .field("retry", &self.inner.retry)
            .field("project", &self.inner.project)
            .finish_non_exhaustive()
    }
}

impl HttpClient {
    /// The API base URL, without a trailing slash.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.inner.base_url
    }

    /// The timeout of each attempt.
    #[must_use]
    pub fn timeout(&self) -> Duration {
        self.inner.timeout
    }

    /// The retry policy.
    #[must_use]
    pub fn retry_policy(&self) -> &RetryPolicy {
        &self.inner.retry
    }

    /// The project header value, when this client is scoped to a project.
    #[must_use]
    pub fn project(&self) -> Option<&str> {
        self.inner.project.as_deref()
    }

    /// A client with the same configuration, scoped to another project.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when `project` is blank or longer than 200 characters.
    pub fn with_project(&self, project: &str) -> Result<Self, Error> {
        let project = normalize_project(Some(project))?.ok_or_else(|| {
            Error::Config("with_project needs a project id or `ext:<externalId>`".to_owned())
        })?;
        Ok(Self {
            inner: Arc::new(Inner {
                api_key: self.inner.api_key.clone(),
                base_url: self.inner.base_url.clone(),
                timeout: self.inner.timeout,
                retry: self.inner.retry.clone(),
                project: Some(project),
                client: self.inner.client.clone(),
            }),
        })
    }

    /// A request for `parts`, answered with `T`. Nothing is sent until the
    /// request is awaited.
    pub fn request<T>(&self, parts: RequestParts) -> Request<T> {
        Request {
            http: self.clone(),
            parts,
            idempotency_key: None,
            timeout: None,
            response: PhantomData,
        }
    }

    /// Sends `parts`, retrying as the policy allows, and returns the body of
    /// the successful response.
    async fn execute(
        &self,
        parts: &RequestParts,
        idempotency_key: Option<String>,
        timeout: Option<Duration>,
    ) -> Result<Success, Error> {
        let inner = &*self.inner;
        let body = match &parts.body {
            Some(Ok(bytes)) => Some(bytes.clone()),
            Some(Err(message)) => return Err(Error::Encode(message.clone())),
            None => None,
        };
        let url = build_url(&inner.base_url, &parts.path, &parts.query);
        let timeout = timeout.unwrap_or(inner.timeout);
        // Operations that take an idempotency key get one on every attempt,
        // so a retry is replayed by the server instead of repeated.
        let key = idempotency_key.or_else(|| parts.keyed.then(random_key));
        let repeatable = parts.keyed || parts.retryable;

        let mut attempt: u32 = 0;
        loop {
            let can_retry = attempt < inner.retry.max_retries;
            let mut request = inner
                .client
                .request(parts.method.into(), &url)
                .timeout(timeout)
                .bearer_auth(&inner.api_key)
                .header("accept", "application/json")
                .header("user-agent", USER_AGENT);
            if let Some(project) = &inner.project {
                request = request.header(PROJECT_HEADER, project);
            }
            if let Some(key) = &key {
                request = request.header(IDEMPOTENCY_HEADER, key);
            }
            if let Some(body) = &body {
                request = request
                    .header("content-type", "application/json")
                    .body(body.clone());
            }

            let received = match request.send().await {
                Ok(response) => {
                    let status = response.status().as_u16();
                    let request_id = header(&response, REQUEST_ID_HEADER);
                    let retry_after = header(&response, "retry-after")
                        .and_then(|value| parse_retry_after(&value, SystemTime::now()));
                    response
                        .bytes()
                        .await
                        .map(|bytes| (status, request_id, retry_after, bytes))
                }
                Err(error) => Err(error),
            };

            let wait = match received {
                Ok((status, request_id, _, bytes)) if (200..300).contains(&status) => {
                    return Ok(Success {
                        status,
                        request_id,
                        body: bytes.to_vec(),
                    });
                }
                Ok((status, request_id, retry_after, bytes)) => {
                    // A Retry-After longer than we would wait (a cooldown)
                    // fails now instead of retrying too early. A 429 was
                    // refused before doing anything, so it is always safe to
                    // send again; a 5xx may have done part of the work.
                    let too_long = retry_after.is_some_and(|w| w > inner.retry.max_retry_after);
                    let retryable = status == 429 || (status >= 500 && repeatable);
                    if !(retryable && can_retry && !too_long) {
                        return Err(api_error(status, request_id, retry_after, &bytes));
                    }
                    retry_after.unwrap_or_else(|| inner.retry.backoff(attempt))
                }
                Err(error) => {
                    // A connection that was never opened sent nothing.
                    if !(can_retry && (repeatable || error.is_connect())) {
                        return Err(if error.is_timeout() {
                            Error::Timeout { timeout }
                        } else {
                            Error::Network(error)
                        });
                    }
                    inner.retry.backoff(attempt)
                }
            };
            tokio::time::sleep(wait).await;
            attempt += 1;
        }
    }
}

struct Success {
    status: u16,
    request_id: Option<String>,
    body: Vec<u8>,
}

fn header(response: &reqwest::Response, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

fn api_error(
    status: u16,
    request_id: Option<String>,
    retry_after: Option<Duration>,
    body: &[u8],
) -> Error {
    let mut code = format!("http_{status}");
    let mut message = format!("Request failed with status {status}.");
    let mut details = None;
    // An empty or non-JSON body keeps the generic code and message.
    if let Ok(serde_json::Value::Object(mut parsed)) = serde_json::from_slice(body) {
        if let Some(serde_json::Value::String(value)) = parsed.remove("code") {
            code = value;
        }
        if let Some(serde_json::Value::String(value)) = parsed.remove("message") {
            message = value;
        }
        if let Some(serde_json::Value::Object(value)) = parsed.remove("details") {
            details = Some(value);
        }
    }
    Error::Api {
        status,
        code,
        message,
        details,
        request_id,
        retry_after,
    }
}

/// `Retry-After`: seconds, or an HTTP date.
fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<f64>() {
        return (seconds.is_finite() && seconds >= 0.0)
            .then(|| Duration::try_from_secs_f64(seconds).ok())
            .flatten();
    }
    let at = parse_http_date(value)?;
    let now = now.duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(Duration::from_secs(at.saturating_sub(now)))
}

/// Seconds since the Unix epoch of an IMF-fixdate (`Wed, 21 Oct 2015 07:28:00 GMT`).
fn parse_http_date(value: &str) -> Option<u64> {
    let mut parts = value.split_ascii_whitespace();
    let (_weekday, day, month, year, time, zone) = (
        parts.next()?,
        parts.next()?,
        parts.next()?,
        parts.next()?,
        parts.next()?,
        parts.next()?,
    );
    if zone != "GMT" || parts.next().is_some() {
        return None;
    }
    let day: u64 = day.parse().ok()?;
    let year: u64 = year.parse().ok()?;
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let month = MONTHS.iter().position(|m| *m == month)? as u64 + 1;
    let mut clock = time.split(':').map(|n| n.parse::<u64>().ok());
    let (hour, minute, second) = (clock.next()??, clock.next()??, clock.next()??);
    if year < 1970 || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    // Days from 1970-01-01 (Howard Hinnant's days_from_civil).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y / 400;
    let year_of_era = y - era * 400;
    let shifted_month = (month + 9) % 12;
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = (era * 146_097 + day_of_era).checked_sub(719_468)?;
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// Percent-encodes everything but unreserved characters (RFC 3986).
fn encode_into(out: &mut String, value: &str) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push(char::from(HEX[usize::from(byte >> 4)]));
            out.push(char::from(HEX[usize::from(byte & 0x0F)]));
        }
    }
}

/// Encodes one path segment (`120363@g.us` becomes `120363%40g.us`).
#[must_use]
pub fn encode_path(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    encode_into(&mut out, segment);
    out
}

fn build_url(base_url: &str, path: &str, query: &[(String, String)]) -> String {
    let mut url = String::with_capacity(base_url.len() + path.len());
    url.push_str(base_url);
    url.push_str(path);
    for (index, (name, value)) in query.iter().enumerate() {
        url.push(if index == 0 { '?' } else { '&' });
        encode_into(&mut url, name);
        url.push('=');
        encode_into(&mut url, value);
    }
    url
}

/// An HTTP method.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Method {
    /// `GET`
    Get,
    /// `POST`
    Post,
    /// `PUT`
    Put,
    /// `PATCH`
    Patch,
    /// `DELETE`
    Delete,
}

impl Method {
    /// The method as written on the wire.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
        }
    }
}

impl From<Method> for reqwest::Method {
    fn from(method: Method) -> Self {
        match method {
            Method::Get => Self::GET,
            Method::Post => Self::POST,
            Method::Put => Self::PUT,
            Method::Patch => Self::PATCH,
            Method::Delete => Self::DELETE,
        }
    }
}

/// What a request sends: method, path, query string, JSON body, and whether
/// it is safe to send again. The generated methods build these; build one
/// yourself to call an endpoint this version of the SDK does not cover.
#[derive(Clone, Debug)]
pub struct RequestParts {
    method: Method,
    path: String,
    query: Vec<(String, String)>,
    body: Option<Result<Vec<u8>, String>>,
    keyed: bool,
    retryable: bool,
    no_content: bool,
}

impl RequestParts {
    /// A request to `path` (starting with `/`, already encoded: see
    /// [`encode_path`]).
    #[must_use]
    pub fn new(method: Method, path: impl Into<String>) -> Self {
        Self {
            method,
            path: path.into(),
            query: Vec::new(),
            body: None,
            keyed: false,
            retryable: false,
            no_content: false,
        }
    }

    /// Adds a query parameter.
    #[must_use]
    pub fn query(mut self, name: &str, value: impl fmt::Display) -> Self {
        self.query.push((name.to_owned(), value.to_string()));
        self
    }

    /// Adds a query parameter when there is a value.
    #[must_use]
    pub fn query_opt(self, name: &str, value: Option<impl fmt::Display>) -> Self {
        match value {
            Some(value) => self.query(name, value),
            None => self,
        }
    }

    /// Sets the JSON body. A value that cannot be serialized fails the
    /// request with [`Error::Encode`] when it is awaited.
    #[must_use]
    pub fn body<B: Serialize + ?Sized>(mut self, body: &B) -> Self {
        self.body = Some(serde_json::to_vec(body).map_err(|e| e.to_string()));
        self
    }

    /// The operation accepts an idempotency key: one is sent on every
    /// attempt (a random one unless the caller gives its own), which makes
    /// the request safe to retry.
    #[must_use]
    pub fn keyed(mut self) -> Self {
        self.keyed = true;
        self
    }

    /// The operation is safe to send again by its HTTP semantics (`GET`,
    /// `PUT`, `DELETE`).
    #[must_use]
    pub fn retryable(mut self) -> Self {
        self.retryable = true;
        self
    }

    /// The success response has no body to parse.
    #[must_use]
    pub fn no_content(mut self) -> Self {
        self.no_content = true;
        self
    }
}

/// A request that has not been sent yet. Await it to send it:
///
/// ```ignore
/// let message = client.messages().send(params).idempotency_key("order-1042").await?;
/// ```
#[must_use = "a request does nothing until it is awaited"]
pub struct Request<T> {
    http: HttpClient,
    parts: RequestParts,
    idempotency_key: Option<String>,
    timeout: Option<Duration>,
    response: PhantomData<fn() -> T>,
}

impl<T> fmt::Debug for Request<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Request")
            .field("parts", &self.parts)
            .field("idempotency_key", &self.idempotency_key)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl<T> Request<T> {
    /// Sent as the idempotency header. Operations that accept one get a
    /// random key when this is not called, so a retried request is replayed,
    /// not repeated. Pass your own to make a request you send again later
    /// (after a crash, from a queue) the same request.
    pub fn idempotency_key(mut self, key: impl Into<String>) -> Self {
        self.idempotency_key = Some(key.into());
        self
    }

    /// The timeout of each attempt of this request, instead of the client's.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }
}

impl<T: DeserializeOwned> Request<T> {
    /// Sends the request. Awaiting the request does the same.
    ///
    /// # Errors
    ///
    /// [`Error`]: a non-2xx response, a timeout, a network failure, or a
    /// response that does not parse.
    pub async fn send(self) -> Result<T, Error> {
        let success = self
            .http
            .execute(&self.parts, self.idempotency_key, self.timeout)
            .await?;
        let body: &[u8] = if self.parts.no_content || success.body.is_empty() {
            b"null"
        } else {
            &success.body
        };
        serde_json::from_slice(body).map_err(|source| Error::Decode {
            status: success.status,
            request_id: success.request_id,
            source,
        })
    }
}

impl<T: DeserializeOwned + Send + 'static> IntoFuture for Request<T> {
    type Output = Result<T, Error>;
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + Send>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(self.send())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_paths_and_queries() {
        assert_eq!(encode_path("120363@g.us"), "120363%40g.us");
        assert_eq!(encode_path("+58 1/é"), "%2B58%201%2F%C3%A9");
        assert_eq!(encode_path("a-_.~"), "a-_.~");
        let query = vec![
            ("q".to_owned(), "são paulo&x".to_owned()),
            ("limit".to_owned(), "20".to_owned()),
        ];
        assert_eq!(
            build_url("https://api.test", "/v1/x", &query),
            "https://api.test/v1/x?q=s%C3%A3o%20paulo%26x&limit=20"
        );
        assert_eq!(
            build_url("https://api.test", "/v1/x", &[]),
            "https://api.test/v1/x"
        );
    }

    #[test]
    fn parses_retry_after() {
        let now = UNIX_EPOCH + Duration::from_secs(1_445_412_470);
        assert_eq!(parse_retry_after("12", now), Some(Duration::from_secs(12)));
        assert_eq!(
            parse_retry_after(" 0.5 ", now),
            Some(Duration::from_millis(500))
        );
        assert_eq!(parse_retry_after("-1", now), None);
        assert_eq!(parse_retry_after("soon", now), None);
        assert_eq!(parse_http_date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(
            parse_http_date("Wed, 21 Oct 2015 07:28:00 GMT"),
            Some(1_445_412_480)
        );
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT", now),
            Some(Duration::from_secs(10))
        );
        // A date in the past means "now".
        assert_eq!(
            parse_retry_after("Thu, 01 Jan 1970 00:00:00 GMT", now),
            Some(Duration::ZERO)
        );
        assert_eq!(parse_http_date("Wed, 21 Oct 2015 07:28:00 PST"), None);
        assert_eq!(parse_http_date("Wed, 21 Foo 2015 07:28:00 GMT"), None);
    }

    #[test]
    fn backoff_grows_with_jitter_and_a_cap() {
        let policy = RetryPolicy::default();
        for (attempt, base) in [(0, 500), (1, 1_000), (2, 2_000), (4, 8_000), (30, 8_000)] {
            let wait = policy.backoff(attempt);
            assert!(
                wait >= Duration::from_millis(base / 2),
                "{attempt}: {wait:?}"
            );
            assert!(wait <= Duration::from_millis(base), "{attempt}: {wait:?}");
        }
        assert_eq!(RetryPolicy::none().max_retries, 0);
        let unit = random_unit();
        assert!((0.0..1.0).contains(&unit));
    }

    #[test]
    fn projects_are_trimmed_and_bounded() {
        assert_eq!(normalize_project(None).unwrap(), None);
        assert_eq!(normalize_project(Some("  ")).unwrap(), None);
        assert_eq!(
            normalize_project(Some(" ext:acme ")).unwrap().as_deref(),
            Some("ext:acme")
        );
        assert!(normalize_project(Some(&"x".repeat(201))).is_err());
    }

    #[test]
    fn debug_output_hides_the_api_key() {
        let builder = ClientBuilder::new().api_key("wu_live_secret");
        assert!(!format!("{builder:?}").contains("wu_live_secret"));
        let http = builder.build_http().unwrap();
        assert!(!format!("{http:?}").contains("wu_live_secret"));
    }

    #[test]
    fn error_bodies_fall_back_to_generic_codes() {
        let error = api_error(502, None, None, b"<html>bad gateway</html>");
        assert_eq!(error.code(), "http_502");
        assert_eq!(error.status(), Some(502));
        let error = api_error(
            400,
            Some("req_1".to_owned()),
            None,
            br#"{"code":"invalid_request","message":"`text` is required.","details":{"field":"text"}}"#,
        );
        assert_eq!(error.code(), "invalid_request");
        assert_eq!(error.request_id(), Some("req_1"));
        assert_eq!(
            error.details().and_then(|d| d.get("field")),
            Some(&"text".into())
        );
        assert_eq!(
            error.to_string(),
            "`text` is required. (400 invalid_request, request req_1)"
        );
    }
}
