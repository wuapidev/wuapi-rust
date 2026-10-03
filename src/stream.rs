// Copied by wuapi-codegen from packages/sdk-codegen/templates/rust/src/stream.rs. Do not edit here.

//! The event stream runtime: one long-lived `GET` answered with
//! `text/event-stream`, reconnected and resumed by itself.
//!
//! It follows `packages/sdk-codegen/streams/CONTRACT.md`, and
//! `tests/api/stream_conformance.rs` holds it to that contract with the
//! scenarios every SDK shares.
//!
//! [`EventStream`] is the stream: ask it for the next event, or turn it into
//! a [`futures_util::Stream`](futures_util::stream::Stream). Everything
//! under it is public too, for a caller that drives the connection itself:
//! the [`StreamTransport`] that opens a connection, the [`SseParser`] that
//! reads one, and the [`StreamBudget`] that paces connects.

use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use futures_util::stream::{self, BoxStream};
use tokio::time::Instant;

use crate::error::Error;
use crate::http::HttpClient;
use crate::meta::{DEFAULT_BASE_URL, PROJECT_HEADER, USER_AGENT};
use crate::types::WebhookEvent;

/// A boxed future, as the transport traits return.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The stream's times.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamTiming {
    /// The reconnection time before the stream sends a `retry:` of its own.
    pub retry: Duration,
    /// How often the server sends a heartbeat comment when idle.
    pub heartbeat: Duration,
    /// Silence after which a connection is taken for dead.
    pub idle_timeout: Duration,
    /// Bound on a connect, up to the response headers.
    pub open_timeout: Duration,
    /// First step of the random part of the reconnect wait.
    pub backoff_base: Duration,
    /// Largest step of the random part of the reconnect wait.
    pub backoff_cap: Duration,
    /// Smallest wait between two connects.
    pub min_gap: Duration,
    /// A connection open this long resets the failure count.
    pub stable_after: Duration,
    /// Clamp on a server-sent `retry:`.
    pub retry_max: Duration,
    /// Clamp on a server-sent `Retry-After`.
    pub retry_after_max: Duration,
    /// How long wuapi keeps events: a cursor older than this never resumes. A resume is guaranteed only within 28 minutes of it.
    pub replay_window: Duration,
}

/// The stream's sizes and counts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamLimits {
    /// Connects one client may start in one `connect_budget_window`.
    pub connect_budget: usize,
    /// The rolling window of `connect_budget`.
    pub connect_budget_window: Duration,
    /// Largest frame accepted, in bytes.
    pub max_frame_bytes: usize,
    /// Event ids remembered to drop a replay's repeats.
    pub dedupe_window: usize,
}

/// What one answer that is not the stream means.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamRefusal {
    /// The HTTP status.
    pub status: u16,
    /// Only for answers whose error body has this `code`.
    pub code: Option<String>,
    /// The terminal error this answer is. Without it the answer is retried.
    pub error: Option<StreamErrorKind>,
    /// Terminal only when the body is the API's error; otherwise retried.
    pub api_only: bool,
    /// The wait before the next connect when the answer has no `Retry-After`.
    pub wait: Option<Duration>,
}

/// The description of a stream: what the generator read from its config.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamSpec {
    /// The stream host's production URL.
    pub base_url: String,
    /// The path of the stream on that host.
    pub path: String,
    /// Request header that carries the cursor to resume from.
    pub resume_header: String,
    /// The `event:` name of the frame that says the cursor is gone.
    pub reset_event: String,
    /// Times.
    pub timing: StreamTiming,
    /// Sizes and counts.
    pub limits: StreamLimits,
    /// What an answer that is not the stream means.
    pub refusals: Vec<StreamRefusal>,
}

/// Why a stream ended for good. Everything else is retried and never an
/// error.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum StreamErrorKind {
    /// The API refused the key (`401`).
    Unauthorized,
    /// The API refused the caller (`403`): a suspended organization.
    Forbidden,
    /// The request is wrong (`400`): a filter the stream does not accept.
    InvalidRequest,
    /// The project the client is scoped to does not exist (`404`).
    NotFound,
    /// Any other answer that waiting does not fix.
    Refused,
    /// A `200` that is not an event stream.
    UnexpectedResponse,
    /// No open connection for the give-up time.
    GaveUp,
}

impl StreamErrorKind {
    /// The kind as the contract spells it (`invalid_request`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unauthorized => "unauthorized",
            Self::Forbidden => "forbidden",
            Self::InvalidRequest => "invalid_request",
            Self::NotFound => "not_found",
            Self::Refused => "refused",
            Self::UnexpectedResponse => "unexpected_response",
            Self::GaveUp => "gave_up",
        }
    }
}

/// A terminal stream failure: the stream has stopped and connects no more,
/// until [`EventStream::restart_after`] is called.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamError {
    /// Why.
    pub kind: StreamErrorKind,
    /// The HTTP status, when the stream host answered.
    pub status: Option<u16>,
    /// The API's error code, when the body was the API's error.
    pub code: Option<String>,
    /// The API's message, or a description of what happened.
    pub message: String,
    /// The `Retry-After` of the answer, when it had one.
    pub retry_after: Option<Duration>,
}

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({}", self.message, self.kind.as_str())?;
        if let Some(status) = self.status {
            write!(f, ", {status}")?;
        }
        if let Some(code) = &self.code {
            write!(f, " {code}")?;
        }
        f.write_str(")")
    }
}

impl std::error::Error for StreamError {}

/// One event of the stream: the same envelope a webhook carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamEvent {
    /// The frame's `event:` field: the event type (`message.received`).
    pub event: String,
    /// The envelope's `id` (`evt_...`), stable across replays.
    pub id: Option<String>,
    /// The cursor after this event: resume from it with `last_event_id`.
    pub cursor: Option<String>,
    /// The frame's `data:`, exactly as received: the envelope as JSON.
    pub data: String,
}

impl StreamEvent {
    /// The event, typed. An event of a type this version of the crate does
    /// not know still parses (as the union's `Unknown`).
    ///
    /// # Errors
    ///
    /// The JSON error, when the envelope does not match the type. The
    /// stream is not affected: [`StreamEvent::data`] still has the text.
    pub fn parse(&self) -> Result<WebhookEvent, serde_json::Error> {
        serde_json::from_str(&self.data)
    }

    /// The event as any type of yours.
    ///
    /// # Errors
    ///
    /// The JSON error, when the envelope does not match `T`.
    pub fn parse_as<T: serde::de::DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_str(&self.data)
    }
}

/// Why a connection ended or a connect failed, when the stream tries again.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ReconnectReason {
    /// The server closed the stream (a deploy, a long run).
    Ended,
    /// Reading the stream failed.
    Cut,
    /// No byte arrived for the idle time.
    Idle,
    /// A connect got no answer.
    Unreachable,
    /// A connect got an answer that is retried (`408`, `429`, `5xx`).
    Refused,
}

impl ReconnectReason {
    /// The reason as the contract spells it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ended => "ended",
            Self::Cut => "cut",
            Self::Idle => "idle",
            Self::Unreachable => "unreachable",
            Self::Refused => "refused",
        }
    }
}

/// The stream is not connected and will connect again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reconnecting {
    /// Why.
    pub reason: ReconnectReason,
    /// The time until the next connect starts.
    pub delay: Duration,
    /// Consecutive failures so far.
    pub failures: u32,
    /// The HTTP status, when `reason` is `Refused`.
    pub status: Option<u16>,
    /// The API's error code, when the body was the API's error.
    pub code: Option<String>,
    /// What went wrong, for a log. Never the key.
    pub message: Option<String>,
    /// The `Retry-After` of the answer, when it had one.
    pub retry_after: Option<Duration>,
}

/// Why a frame was dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SkipReason {
    /// The frame is larger than the stream accepts.
    Oversized,
    /// The frame's data is not a JSON object.
    InvalidData,
}

impl SkipReason {
    /// The reason as the contract spells it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Oversized => "oversized",
            Self::InvalidData => "invalid_data",
        }
    }
}

/// Something a stream reports, in the order it happens.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum StreamItem {
    /// A connection reached the stream.
    Open,
    /// An event.
    Event(StreamEvent),
    /// The cursor could not be resumed: events may have been missed. Resync
    /// over REST; the stream goes on live.
    Reset {
        /// `cursor_expired`, `cursor_unknown`, `not_logged`, or `unknown`.
        reason: String,
    },
    /// A frame was dropped.
    Skipped {
        /// Why.
        reason: SkipReason,
        /// The frame's `event:` field, when it is known.
        event: Option<String>,
    },
    /// Not connected; the next connect starts after its `delay`.
    Reconnecting(Reconnecting),
}

/// Where a stream is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StreamState {
    /// Waiting to connect.
    Waiting,
    /// A connect is under way.
    Connecting,
    /// A connection is open.
    Open,
    /// Not connecting: closed, or stopped by a terminal error.
    Stopped,
}

// ---------------------------------------------------------------------------
// The parser.

/// What the parser found in the bytes it was fed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SseItem {
    /// A dispatched frame.
    Event {
        /// The `event:` field, `message` when the frame has none.
        event: String,
        /// The `data:` lines, joined with a line feed.
        data: String,
        /// The frame's own `id:`.
        id: Option<String>,
    },
    /// A block with an id and no data.
    Cursor(String),
    /// A `retry:` hint.
    Retry(Duration),
    /// A comment line.
    Comment,
    /// A frame over the cap was dropped.
    Oversized,
}

/// The byte order mark some servers put first.
const BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

/// Incremental parser of the event stream format.
///
/// It splits on bytes and decodes one complete line at a time, so a chunk
/// may cut a line, a CRLF or a character anywhere: the result is the same
/// for every split. A half-read frame is never dispatched: drop the parser
/// with the connection.
#[derive(Debug)]
pub struct SseParser {
    max_frame: usize,
    /// The first bytes, while they could still be a byte order mark.
    head: Vec<u8>,
    started: bool,
    line: Vec<u8>,
    /// The line outgrew the cap: its bytes are skipped to its end.
    line_overflow: bool,
    /// A CR ended the last line: an LF right after it is the same line end.
    pending_cr: bool,
    event: String,
    data: String,
    has_data: bool,
    id: Option<String>,
    frame_bytes: usize,
    poisoned: bool,
}

impl SseParser {
    /// A parser that drops frames and lines over `max_frame_bytes`.
    #[must_use]
    pub fn new(max_frame_bytes: usize) -> Self {
        Self {
            max_frame: max_frame_bytes,
            head: Vec::new(),
            started: false,
            line: Vec::new(),
            line_overflow: false,
            pending_cr: false,
            event: String::new(),
            data: String::new(),
            has_data: false,
            id: None,
            frame_bytes: 0,
            poisoned: false,
        }
    }

    /// Takes the next bytes and returns what they completed.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseItem> {
        let mut items = Vec::new();
        for &byte in chunk {
            if self.started {
                self.byte(byte, &mut items);
                continue;
            }
            self.head.push(byte);
            if self.head == BOM {
                self.head.clear();
                self.started = true;
            } else if !BOM.starts_with(&self.head) {
                self.started = true;
                for held in std::mem::take(&mut self.head) {
                    self.byte(held, &mut items);
                }
            }
        }
        items
    }

    fn byte(&mut self, byte: u8, items: &mut Vec<SseItem>) {
        if std::mem::take(&mut self.pending_cr) && byte == b'\n' {
            return;
        }
        match byte {
            b'\n' => self.end_line(items),
            b'\r' => {
                self.pending_cr = true;
                self.end_line(items);
            }
            _ if self.line_overflow => {}
            _ => {
                self.line.push(byte);
                if self.line.len() > self.max_frame {
                    self.line.clear();
                    self.line_overflow = true;
                }
            }
        }
    }

    fn end_line(&mut self, items: &mut Vec<SseItem>) {
        let line = std::mem::take(&mut self.line);
        if std::mem::take(&mut self.line_overflow) {
            self.poison();
            return;
        }
        if line.is_empty() {
            self.dispatch(items);
            return;
        }
        self.frame_bytes += line.len() + 1;
        if self.frame_bytes > self.max_frame {
            self.poison();
        }
        let line = String::from_utf8_lossy(&line);
        if line.starts_with(':') {
            items.push(SseItem::Comment);
            return;
        }
        let Some((name, value)) = line.split_once(':') else {
            return;
        };
        let value = value.strip_prefix(' ').unwrap_or(value);
        match name {
            // Even a dropped frame has its id honoured, so the cursor
            // moves past it.
            "id" if !value.contains('\0') => self.id = Some(value.to_owned()),
            "retry" if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) => {
                // More digits than fit is still a number: the largest.
                let millis = value.parse().unwrap_or(u64::MAX);
                items.push(SseItem::Retry(Duration::from_millis(millis)));
            }
            "event" if !self.poisoned => value.clone_into(&mut self.event),
            "data" if !self.poisoned => {
                if self.has_data {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.has_data = true;
            }
            _ => {}
        }
    }

    /// The frame is over the cap: its content goes, its id stays.
    fn poison(&mut self) {
        self.poisoned = true;
        self.event.clear();
        self.data.clear();
        self.has_data = false;
    }

    /// A blank line: the block is complete.
    fn dispatch(&mut self, items: &mut Vec<SseItem>) {
        let event = std::mem::take(&mut self.event);
        let data = std::mem::take(&mut self.data);
        let id = self.id.take();
        let has_data = std::mem::take(&mut self.has_data);
        self.frame_bytes = 0;
        if std::mem::take(&mut self.poisoned) {
            items.push(SseItem::Oversized);
        } else if has_data {
            items.push(SseItem::Event {
                event: if event.is_empty() {
                    "message".to_owned()
                } else {
                    event
                },
                data,
                id,
            });
            return;
        }
        if let Some(id) = id {
            items.push(SseItem::Cursor(id));
        }
    }
}

// ---------------------------------------------------------------------------
// The transport.

/// An answer to a connect that is not the stream. The body is capped.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Refused {
    /// The HTTP status.
    pub status: u16,
    /// The `Content-Type` header.
    pub content_type: Option<String>,
    /// The body, at most 64 KiB of it.
    pub body: Vec<u8>,
    /// The `Retry-After` header.
    pub retry_after: Option<Duration>,
}

/// What a connect came to, when it got an answer.
pub enum Answer {
    /// `200` with `Content-Type: text/event-stream`.
    Stream(Box<dyn StreamBody>),
    /// Anything else, a `200` of another type included.
    Refused(Refused),
}

impl fmt::Debug for Answer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stream(_) => f.write_str("Stream(..)"),
            Self::Refused(refused) => f.debug_tuple("Refused").field(refused).finish(),
        }
    }
}

/// The body of an open stream.
pub trait StreamBody: Send {
    /// The next bytes, `None` at a clean end, `Err` when the connection
    /// broke. It must lose nothing when its future is dropped: the stream
    /// polls it against timers.
    fn chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, String>>;
}

/// Opens the stream. `Err` is a connect that got no answer: network, TLS.
///
/// The real one is [`HttpStreamTransport`]; a test replaces it to script a
/// server.
pub trait StreamTransport: Send + Sync {
    /// One connect, with the cursor to resume from when there is one. The
    /// future owns what it needs, so the stream can hold it across polls.
    fn connect(&self, last_event_id: Option<String>) -> BoxFuture<'static, Result<Answer, String>>;
}

/// The request one connect sends.
#[derive(Clone, PartialEq, Eq)]
pub struct ConnectRequest {
    /// The full URL, filters included. Never the key.
    pub url: String,
    /// The headers, the `Authorization` header included.
    pub headers: Vec<(String, String)>,
}

impl ConnectRequest {
    /// The value of a header, by its name in any case.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

impl fmt::Debug for ConnectRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let headers: Vec<(&str, &str)> = self
            .headers
            .iter()
            .map(|(n, v)| {
                let shown = if n.eq_ignore_ascii_case("authorization") {
                    "<redacted>"
                } else {
                    v.as_str()
                };
                (n.as_str(), shown)
            })
            .collect();
        f.debug_struct("ConnectRequest")
            .field("url", &self.url)
            .field("headers", &headers)
            .finish()
    }
}

/// How much of a refusal's body is read: an error body is a few hundred
/// bytes.
const REFUSED_BODY_MAX: usize = 64 * 1024;

/// The stream request has no total timeout of its own: an open stream is
/// read for as long as it lasts. This replaces a total timeout the caller's
/// `reqwest` client may carry, with one far longer than a connection lives.
const NO_TOTAL_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

/// The real transport: the client's own `reqwest` client, key and project.
///
/// `reqwest` follows a redirect, and drops the `Authorization` header when
/// the redirect leaves the host: the key never follows one to another
/// origin.
#[derive(Clone)]
pub struct HttpStreamTransport {
    http: HttpClient,
    url: String,
    resume_header: String,
    user_agent: String,
}

impl fmt::Debug for HttpStreamTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpStreamTransport")
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

impl HttpStreamTransport {
    /// A transport for `spec` with these filters: query parameter and
    /// values, each sent comma-joined and left out when empty.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when the stream URL is not one the key may travel
    /// to (see [`check_stream_url`]).
    pub fn new(
        http: &HttpClient,
        spec: &StreamSpec,
        filters: &[(&str, Vec<String>)],
    ) -> Result<Self, Error> {
        let root = stream_root(http, spec)?;
        let query: Vec<(String, String)> = filters
            .iter()
            .filter(|(_, values)| !values.is_empty())
            .map(|(name, values)| ((*name).to_owned(), values.join(",")))
            .collect();
        Ok(Self {
            http: http.clone(),
            url: crate::http::build_url(&root, &spec.path, &query),
            resume_header: spec.resume_header.clone(),
            user_agent: USER_AGENT.to_owned(),
        })
    }

    /// The `User-Agent` of the connects, instead of this crate's: the name
    /// an application wants its stream connections listed under.
    #[must_use]
    pub fn user_agent(mut self, user_agent: impl Into<String>) -> Self {
        self.user_agent = user_agent.into();
        self
    }

    /// The URL every connect goes to.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The request a connect sends, with this cursor.
    #[must_use]
    pub fn request(&self, last_event_id: Option<&str>) -> ConnectRequest {
        let mut headers = vec![
            (
                "authorization".to_owned(),
                format!("Bearer {}", self.http.api_key()),
            ),
            ("accept".to_owned(), "text/event-stream".to_owned()),
            ("cache-control".to_owned(), "no-cache".to_owned()),
            ("user-agent".to_owned(), self.user_agent.clone()),
        ];
        if let Some(project) = self.http.project() {
            headers.push((PROJECT_HEADER.to_ascii_lowercase(), project.to_owned()));
        }
        if let Some(id) = last_event_id {
            headers.push((self.resume_header.to_ascii_lowercase(), id.to_owned()));
        }
        ConnectRequest {
            url: self.url.clone(),
            headers,
        }
    }
}

impl StreamTransport for HttpStreamTransport {
    fn connect(&self, last_event_id: Option<String>) -> BoxFuture<'static, Result<Answer, String>> {
        let request = self.request(last_event_id.as_deref());
        let mut builder = self
            .http
            .reqwest()
            .get(&request.url)
            .timeout(NO_TOTAL_TIMEOUT);
        for (name, value) in &request.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        Box::pin(async move {
            let mut response = builder.send().await.map_err(describe)?;
            let status = response.status().as_u16();
            let header = |name: &str| {
                response
                    .headers()
                    .get(name)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned)
            };
            let content_type = header("content-type");
            let retry_after = header("retry-after")
                .and_then(|v| crate::http::parse_retry_after(&v, SystemTime::now()));
            let is_stream = content_type.as_deref().is_some_and(|value| {
                value
                    .trim_start()
                    .to_ascii_lowercase()
                    .starts_with("text/event-stream")
            });
            if status == 200 && is_stream {
                return Ok(Answer::Stream(Box::new(HttpBody(response))));
            }
            let mut body = Vec::new();
            while body.len() < REFUSED_BODY_MAX {
                match response.chunk().await {
                    Ok(Some(bytes)) => body.extend_from_slice(&bytes),
                    _ => break,
                }
            }
            body.truncate(REFUSED_BODY_MAX);
            Ok(Answer::Refused(Refused {
                status,
                content_type,
                body,
                retry_after,
            }))
        })
    }
}

/// What failed, without the URL.
fn describe(error: reqwest::Error) -> String {
    error.without_url().to_string()
}

struct HttpBody(reqwest::Response);

impl StreamBody for HttpBody {
    fn chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, String>> {
        Box::pin(async move {
            self.0
                .chunk()
                .await
                .map(|bytes| bytes.map(|bytes| bytes.to_vec()))
                .map_err(describe)
        })
    }
}

/// The scheme and authority of a URL (`https://host:port`).
fn origin(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    (!scheme.is_empty() && !authority.is_empty()).then(|| format!("{scheme}://{authority}"))
}

/// Where the stream is served for this client: its own stream URL; the
/// description's when it talks to the API's production URL; otherwise the
/// origin of its API URL. A key configured for another API host is never
/// sent to the production stream host.
fn stream_root(http: &HttpClient, spec: &StreamSpec) -> Result<String, Error> {
    let root = match http.stream_base_url() {
        Some(root) => root.to_owned(),
        None if http.base_url().eq_ignore_ascii_case(DEFAULT_BASE_URL) => spec.base_url.clone(),
        None => origin(http.base_url())
            .ok_or_else(|| Error::Config("the API base URL has no usable origin".to_owned()))?,
    };
    let root = root.trim_end_matches('/').to_owned();
    check_stream_url(&root).map_err(|why| Error::Config(format!("the stream URL: {why}")))?;
    Ok(root)
}

/// Whether the stream can be read from `url`: `Err` says why not.
///
/// The API key travels to it, so: `https` anywhere, `http` only on this
/// machine (`localhost`, `127.0.0.0/8`, `[::1]`), and no user info, query or
/// fragment.
///
/// # Errors
///
/// What is wrong with the URL, in words.
pub fn check_stream_url(url: &str) -> Result<(), String> {
    let (scheme, rest) = url.split_once("://").ok_or("it must start with https://")?;
    if rest.contains(['?', '#']) {
        return Err("it must not carry a query or a fragment".to_owned());
    }
    let authority = rest.split('/').next().unwrap_or_default();
    if authority.contains('@') {
        return Err("it must not carry a user name or password".to_owned());
    }
    let (host, port) = match authority.strip_prefix('[') {
        Some(inner) => {
            let (host, after) = inner.split_once(']').ok_or("the address is not valid")?;
            (format!("[{host}]"), after.strip_prefix(':'))
        }
        None => match authority.rsplit_once(':') {
            Some((host, port)) => (host.to_owned(), Some(port)),
            None => (authority.to_owned(), None),
        },
    };
    if host.is_empty() || host == "[]" {
        return Err("it has no host".to_owned());
    }
    if port.is_some_and(|port| port.parse::<u16>().is_err()) {
        return Err("the port is not valid".to_owned());
    }
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host == "[::1]"
        || host
            .parse::<std::net::Ipv4Addr>()
            .is_ok_and(|ip| ip.is_loopback());
    match scheme.to_ascii_lowercase().as_str() {
        "https" => Ok(()),
        "http" if loopback => Ok(()),
        "http" => Err("plain http is only for this machine; use https://".to_owned()),
        _ => Err("it must start with https://".to_owned()),
    }
}

// ---------------------------------------------------------------------------
// The connect budget.

fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// How many connects a rolling window allows. Clones share one count: every
/// stream of one client draws on the same budget, so a caller that opens
/// streams in a loop is paced like one that keeps a single stream.
#[derive(Clone, Debug)]
pub struct StreamBudget {
    starts: Arc<Mutex<Vec<Instant>>>,
    limit: usize,
    window: Duration,
}

impl StreamBudget {
    /// A budget of its own, with these limits.
    #[must_use]
    pub fn new(limits: &StreamLimits) -> Self {
        Self::shared(Arc::default(), limits)
    }

    /// The budget of a client: every stream it opens, and every stream of
    /// its project-scoped copies, draws on this one.
    #[must_use]
    pub fn of(http: &HttpClient, limits: &StreamLimits) -> Self {
        Self::shared(http.stream_connects(), limits)
    }

    /// A budget over an existing count.
    fn shared(starts: Arc<Mutex<Vec<Instant>>>, limits: &StreamLimits) -> Self {
        Self {
            starts,
            limit: limits.connect_budget.max(1),
            window: limits.connect_budget_window,
        }
    }

    /// The earliest a connect planned for `at` could start. Books nothing.
    #[must_use]
    pub fn next_slot(&self, at: Instant) -> Instant {
        Self::slot(&locked(&self.starts), at, self.limit, self.window)
    }

    /// Books the earliest start at or after `at` that keeps every window
    /// within the limit, and returns it. Booked at once, so streams that
    /// plan at the same instant do not all take the same slot.
    pub fn reserve(&self, at: Instant) -> Instant {
        let now = Instant::now();
        let mut starts = locked(&self.starts);
        starts.retain(|start| now.saturating_duration_since(*start) < self.window);
        let slot = Self::slot(&starts, at, self.limit, self.window);
        starts.push(slot);
        slot
    }

    fn slot(starts: &[Instant], at: Instant, limit: usize, window: Duration) -> Instant {
        let mut slot = at;
        loop {
            let mut near: Vec<Instant> = starts
                .iter()
                .copied()
                .filter(|start| {
                    let apart = if slot >= *start {
                        slot - *start
                    } else {
                        *start - slot
                    };
                    apart < window
                })
                .collect();
            if near.len() < limit {
                return slot;
            }
            near.sort();
            // The start that frees a slot is the one `limit` back.
            slot = near[near.len() - limit] + window;
        }
    }
}

// ---------------------------------------------------------------------------
// The stream.

/// A number in `[0, 1)`, for the random part of a wait.
pub type Jitter = Arc<dyn Fn() -> f64 + Send + Sync>;

/// Called for every item that is not an event.
pub type OnStatus = Arc<dyn Fn(&StreamItem) + Send + Sync>;

/// How one stream is run. Every field has a default: the stream's own.
#[derive(Clone, Default)]
pub struct StreamOptions {
    dedupe_window: Option<usize>,
    max_frame_bytes: Option<usize>,
    give_up_after: Option<Duration>,
    start_after: Option<Duration>,
    jitter: Option<Jitter>,
    on_status: Option<OnStatus>,
    budget: Option<StreamBudget>,
}

impl fmt::Debug for StreamOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamOptions")
            .field("dedupe_window", &self.dedupe_window)
            .field("max_frame_bytes", &self.max_frame_bytes)
            .field("give_up_after", &self.give_up_after)
            .field("start_after", &self.start_after)
            .finish_non_exhaustive()
    }
}

impl StreamOptions {
    /// The defaults.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Event ids remembered to drop a replay's repeats. `0` delivers every
    /// frame.
    #[must_use]
    pub fn dedupe_window(mut self, ids: usize) -> Self {
        self.dedupe_window = Some(ids);
        self
    }

    /// Largest frame accepted, in bytes.
    #[must_use]
    pub fn max_frame_bytes(mut self, bytes: usize) -> Self {
        self.max_frame_bytes = Some(bytes);
        self
    }

    /// Stop with [`StreamErrorKind::GaveUp`] after this long without an open
    /// connection. Without it the stream retries for ever.
    #[must_use]
    pub fn give_up_after(mut self, after: Duration) -> Self {
        self.give_up_after = Some(after);
        self
    }

    /// The first connect starts this long from now, instead of at once.
    #[must_use]
    pub fn start_after(mut self, wait: Duration) -> Self {
        self.start_after = Some(wait);
        self
    }

    /// Called for every item that is not an event, by
    /// [`EventStream::next_event`] and the stream of events, which do not
    /// return them.
    #[must_use]
    pub fn on_status(mut self, on_status: impl Fn(&StreamItem) + Send + Sync + 'static) -> Self {
        self.on_status = Some(Arc::new(on_status));
        self
    }

    /// The random number of the reconnect wait, in `[0, 1)`. For tests.
    #[must_use]
    pub fn jitter(mut self, jitter: impl Fn() -> f64 + Send + Sync + 'static) -> Self {
        self.jitter = Some(Arc::new(jitter));
        self
    }

    /// The connect budget to draw on, instead of a new one.
    #[must_use]
    pub fn budget(mut self, budget: StreamBudget) -> Self {
        self.budget = Some(budget);
        self
    }

    /// The connect budget to draw on, unless one was chosen already.
    #[must_use]
    pub fn or_budget(mut self, budget: StreamBudget) -> Self {
        self.budget.get_or_insert(budget);
        self
    }
}

enum State {
    /// Waits until the instant, then connects.
    Waiting(Instant),
    Connecting {
        attempt: BoxFuture<'static, Result<Answer, String>>,
        deadline: Instant,
    },
    Open {
        body: Box<dyn StreamBody>,
        since: Instant,
        last_byte: Instant,
    },
    /// Not connecting: closed, or stopped by a terminal error.
    Stopped,
}

/// An event stream. It connects, reads, reconnects with its cursor and
/// waits between connects by itself, and stops only when it is closed or on
/// a terminal [`StreamError`].
///
/// ```ignore
/// let mut stream = client.events().stream(params)?;
/// while let Some(event) = stream.next_event().await {
///     let event = event?;
///     println!("{} {}", event.event, event.data);
/// }
/// ```
///
/// [`next_event`](Self::next_event) and [`next_item`](Self::next_item) lose
/// nothing when their future is dropped, so a caller can race them against
/// anything. Dropping the stream closes its connection.
#[must_use = "a stream does nothing until it is asked for its next item"]
pub struct EventStream {
    transport: Arc<dyn StreamTransport>,
    spec: StreamSpec,
    dedupe_window: usize,
    max_frame: usize,
    give_up_after: Option<Duration>,
    jitter: Jitter,
    on_status: Option<OnStatus>,
    budget: StreamBudget,
    parser: SseParser,
    cursor: Option<String>,
    /// The last `retry:` the server sent, or the description's.
    retry: Duration,
    failures: u32,
    state: State,
    queue: VecDeque<Result<StreamItem, StreamError>>,
    /// Since when no connection is open; `None` while one is.
    down_since: Option<Instant>,
    seen: HashSet<String>,
    seen_order: VecDeque<String>,
}

impl fmt::Debug for EventStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventStream")
            .field("state", &self.state())
            .field("cursor", &self.cursor)
            .field("failures", &self.failures)
            .finish_non_exhaustive()
    }
}

impl EventStream {
    /// A stream over `transport`, starting from `last_event_id` when given.
    /// The generated `stream` method calls this with the real transport;
    /// call it yourself to change the description's timings or to script
    /// the server in a test.
    pub fn with_transport(
        transport: Arc<dyn StreamTransport>,
        spec: StreamSpec,
        last_event_id: Option<String>,
        options: StreamOptions,
    ) -> Self {
        let budget = options
            .budget
            .unwrap_or_else(|| StreamBudget::new(&spec.limits));
        let now = Instant::now();
        let first = budget.reserve(now + options.start_after.unwrap_or_default());
        let max_frame = options
            .max_frame_bytes
            .unwrap_or(spec.limits.max_frame_bytes);
        Self {
            transport,
            dedupe_window: options.dedupe_window.unwrap_or(spec.limits.dedupe_window),
            max_frame,
            give_up_after: options.give_up_after,
            jitter: options
                .jitter
                .unwrap_or_else(|| Arc::new(crate::http::random_unit)),
            on_status: options.on_status,
            budget,
            parser: SseParser::new(max_frame),
            cursor: last_event_id.filter(|id| !id.is_empty()),
            retry: spec.timing.retry,
            failures: 0,
            state: State::Waiting(first),
            queue: VecDeque::new(),
            down_since: Some(now),
            seen: HashSet::new(),
            seen_order: VecDeque::new(),
            spec,
        }
    }

    /// The cursor of the last frame read: start a new stream from it to
    /// resume here.
    #[must_use]
    pub fn last_event_id(&self) -> Option<&str> {
        self.cursor.as_deref()
    }

    /// Where the stream is.
    #[must_use]
    pub fn state(&self) -> StreamState {
        match self.state {
            State::Waiting(_) => StreamState::Waiting,
            State::Connecting { .. } => StreamState::Connecting,
            State::Open { .. } => StreamState::Open,
            State::Stopped => StreamState::Stopped,
        }
    }

    /// Whether the stream has stopped: closed, or ended by a terminal error.
    #[must_use]
    pub fn is_stopped(&self) -> bool {
        matches!(self.state, State::Stopped)
    }

    /// Consecutive failures so far.
    #[must_use]
    pub fn failures(&self) -> u32 {
        self.failures
    }

    /// Since when no connection is open, `None` while one is.
    #[must_use]
    pub fn down_since(&self) -> Option<Instant> {
        self.down_since
    }

    /// Ends the stream: its connection is closed, nothing more is reported
    /// and no connect starts afterwards.
    pub fn close(&mut self) {
        self.state = State::Stopped;
        self.queue.clear();
    }

    /// Forgets the cursor: the next connect starts from now.
    pub fn clear_cursor(&mut self) {
        self.cursor = None;
    }

    /// Drops whatever the stream is doing and connects again after `wait`,
    /// with its cursor or without. This is how a stopped stream is started
    /// again; the connect budget still applies.
    pub fn restart_after(&mut self, wait: Duration, keep_cursor: bool) {
        if !keep_cursor {
            self.cursor = None;
        }
        let now = Instant::now();
        // An open connection goes with the old state.
        self.down_since.get_or_insert(now);
        self.state = State::Waiting(self.budget.reserve(now + wait));
    }

    /// The next event. `None` when the stream has stopped; `Some(Err(_))`
    /// once, for the terminal error that stopped it. Everything else the
    /// stream reports goes to [`StreamOptions::on_status`].
    pub async fn next_event(&mut self) -> Option<Result<StreamEvent, StreamError>> {
        loop {
            match self.next_item().await? {
                Ok(StreamItem::Event(event)) => return Some(Ok(event)),
                Ok(item) => {
                    if let Some(on_status) = &self.on_status {
                        on_status(&item);
                    }
                }
                Err(error) => return Some(Err(error)),
            }
        }
    }

    /// The next thing the stream reports: an event, or `Open`, `Reset`,
    /// `Skipped` and `Reconnecting`. `None` when the stream has stopped;
    /// `Some(Err(_))` once, for the terminal error that stopped it.
    pub async fn next_item(&mut self) -> Option<Result<StreamItem, StreamError>> {
        loop {
            if let Some(item) = self.queue.pop_front() {
                return Some(item);
            }
            if self.is_stopped() {
                return None;
            }
            let limit = self.give_up_at();
            if limit.is_some_and(|limit| Instant::now() >= limit) {
                self.state = State::Stopped;
                return Some(Err(StreamError {
                    kind: StreamErrorKind::GaveUp,
                    status: None,
                    code: None,
                    message: "the stream could not be opened for the give-up time".to_owned(),
                    retry_after: None,
                }));
            }
            let bounded = |at: Instant| limit.map_or(at, |limit| limit.min(at));
            match &mut self.state {
                State::Stopped => {}
                State::Waiting(at) => {
                    let at = *at;
                    tokio::time::sleep_until(bounded(at)).await;
                    if Instant::now() >= at {
                        self.state = State::Connecting {
                            attempt: self.transport.connect(self.cursor.clone()),
                            deadline: Instant::now() + self.spec.timing.open_timeout,
                        };
                    }
                }
                State::Connecting { attempt, deadline } => {
                    let deadline = *deadline;
                    match tokio::time::timeout_at(bounded(deadline), attempt).await {
                        Ok(answer) => self.answered(answer),
                        Err(_) if Instant::now() >= deadline => {
                            self.answered(Err("the connection took too long".to_owned()));
                        }
                        // The give-up time came first: said above.
                        Err(_) => {}
                    }
                }
                State::Open {
                    body, last_byte, ..
                } => {
                    let idle_at = *last_byte + self.spec.timing.idle_timeout;
                    match tokio::time::timeout_at(idle_at, body.chunk()).await {
                        Ok(Ok(Some(bytes))) => {
                            *last_byte = Instant::now();
                            self.feed(&bytes);
                        }
                        Ok(Ok(None)) => self.dropped(ReconnectReason::Ended, None),
                        Ok(Err(why)) => self.dropped(ReconnectReason::Cut, Some(why)),
                        Err(_) => self.dropped(ReconnectReason::Idle, None),
                    }
                }
            }
        }
    }

    /// The events as a `Stream`, for combinators. It ends when the stream
    /// stops, after the terminal error if there was one.
    pub fn into_stream(self) -> BoxStream<'static, Result<StreamEvent, StreamError>> {
        Box::pin(stream::unfold(self, |mut this| async move {
            this.next_event().await.map(|event| (event, this))
        }))
    }

    /// Everything the stream reports as a `Stream`.
    pub fn into_items(self) -> BoxStream<'static, Result<StreamItem, StreamError>> {
        Box::pin(stream::unfold(self, |mut this| async move {
            this.next_item().await.map(|item| (item, this))
        }))
    }

    /// The instant the stream gives up at, when it has a give-up time and no
    /// open connection.
    fn give_up_at(&self) -> Option<Instant> {
        Some(self.down_since? + self.give_up_after?)
    }

    fn answered(&mut self, answer: Result<Answer, String>) {
        let now = Instant::now();
        match answer {
            Ok(Answer::Stream(body)) => {
                self.queue.push_back(Ok(StreamItem::Open));
                self.down_since = None;
                self.parser = SseParser::new(self.max_frame);
                self.state = State::Open {
                    body,
                    since: now,
                    last_byte: now,
                };
            }
            Ok(Answer::Refused(refused)) => self.refused(refused),
            Err(why) => {
                self.failures = self.failures.saturating_add(1);
                self.schedule(ReconnectReason::Unreachable, None, None, Some(why), None);
            }
        }
    }

    /// What one answer that is not the stream comes to: a terminal error,
    /// or a wait.
    fn refused(&mut self, refused: Refused) {
        let Refused {
            status,
            retry_after,
            body,
            ..
        } = refused;
        let api = api_error(&body);
        let code = api.as_ref().map(|(code, _)| code.as_str());
        let entries = || self.spec.refusals.iter().filter(|r| r.status == status);
        let entry = entries()
            .find(|r| r.code.is_some() && r.code.as_deref() == code)
            .or_else(|| entries().find(|r| r.code.is_none()));
        let kind = match entry.and_then(|entry| entry.error.map(|kind| (kind, entry.api_only))) {
            _ if status == 200 => Some(StreamErrorKind::UnexpectedResponse),
            Some((kind, api_only)) => (!api_only || api.is_some()).then_some(kind),
            None if retries(status) => None,
            None => Some(StreamErrorKind::Refused),
        };
        let asked = retry_after.or(entry.and_then(|entry| entry.wait));
        let (code, message) = api.unzip();
        if let Some(kind) = kind {
            self.state = State::Stopped;
            self.queue.push_back(Err(StreamError {
                kind,
                status: Some(status),
                code,
                message: message.unwrap_or_else(|| format!("the stream answered {status}")),
                retry_after,
            }));
            return;
        }
        self.failures = self.failures.saturating_add(1);
        self.schedule(
            ReconnectReason::Refused,
            Some(status),
            code,
            message,
            asked.map(|asked| (asked, retry_after)),
        );
    }

    /// The connection ended.
    fn dropped(&mut self, reason: ReconnectReason, message: Option<String>) {
        let now = Instant::now();
        if let State::Open { since, .. } = &self.state {
            self.failures =
                if now.saturating_duration_since(*since) >= self.spec.timing.stable_after {
                    0
                } else {
                    self.failures.saturating_add(1)
                };
        }
        self.down_since = Some(now);
        self.schedule(reason, None, None, message, None);
    }

    /// Plans the next connect (CONTRACT.md, section 5) and says so. `asked`
    /// is what a refusal asked to wait, and the `Retry-After` it came with.
    fn schedule(
        &mut self,
        reason: ReconnectReason,
        status: Option<u16>,
        code: Option<String>,
        message: Option<String>,
        asked: Option<(Duration, Option<Duration>)>,
    ) {
        let timing = &self.spec.timing;
        let wait = asked.map_or(Duration::ZERO, |(wait, _)| wait.min(timing.retry_after_max));
        let floor = self.retry.min(timing.retry_max).max(wait);
        let doublings = self.failures.max(1) - 1;
        let step = timing
            .backoff_base
            .saturating_mul(1u32.checked_shl(doublings).unwrap_or(u32::MAX))
            .min(timing.backoff_cap);
        let draw = (self.jitter)().clamp(0.0, 1.0) * step.as_millis() as f64;
        // Whole milliseconds, the random part truncated.
        let delay =
            Duration::from_millis(floor.as_millis() as u64 + draw as u64).max(timing.min_gap);
        let now = Instant::now();
        let at = self.budget.reserve(now + delay);
        self.state = State::Waiting(at);
        self.queue
            .push_back(Ok(StreamItem::Reconnecting(Reconnecting {
                reason,
                delay: at.saturating_duration_since(now),
                failures: self.failures,
                status,
                code,
                message,
                retry_after: asked.and_then(|(_, retry_after)| retry_after),
            })));
    }

    fn feed(&mut self, bytes: &[u8]) {
        for item in self.parser.feed(bytes) {
            match item {
                SseItem::Comment => {}
                SseItem::Retry(wait) => self.retry = wait,
                SseItem::Cursor(id) => self.set_cursor(Some(id)),
                SseItem::Oversized => self.queue.push_back(Ok(StreamItem::Skipped {
                    reason: SkipReason::Oversized,
                    event: None,
                })),
                SseItem::Event { event, data, id } => {
                    if event == self.spec.reset_event {
                        self.cursor = None;
                        self.queue.push_back(Ok(StreamItem::Reset {
                            reason: reset_reason(&data),
                        }));
                        continue;
                    }
                    self.set_cursor(id);
                    let Ok(envelope) =
                        serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&data)
                    else {
                        self.queue.push_back(Ok(StreamItem::Skipped {
                            reason: SkipReason::InvalidData,
                            event: Some(event),
                        }));
                        continue;
                    };
                    let id = envelope
                        .get("id")
                        .and_then(|id| id.as_str())
                        .map(str::to_owned);
                    if let Some(id) = &id {
                        if self.dedupe_window > 0 && !self.remember(id) {
                            continue;
                        }
                    }
                    self.queue.push_back(Ok(StreamItem::Event(StreamEvent {
                        event,
                        id,
                        cursor: self.cursor.clone(),
                        data,
                    })));
                }
            }
        }
    }

    /// Remembers an event id. `false` when it was delivered not long ago.
    fn remember(&mut self, id: &str) -> bool {
        if !self.seen.insert(id.to_owned()) {
            return false;
        }
        self.seen_order.push_back(id.to_owned());
        if self.seen_order.len() > self.dedupe_window {
            if let Some(oldest) = self.seen_order.pop_front() {
                self.seen.remove(&oldest);
            }
        }
        true
    }

    /// An empty id clears the cursor, as the format says.
    fn set_cursor(&mut self, id: Option<String>) {
        match id {
            Some(id) if id.is_empty() => self.cursor = None,
            Some(id) => self.cursor = Some(id),
            None => {}
        }
    }
}

/// Whether a status the description does not name is retried.
fn retries(status: u16) -> bool {
    matches!(status, 408 | 429 | 500..=599)
}

/// The API's error, when the body is one: a JSON object with a string
/// `code` and a string `message`.
fn api_error(body: &[u8]) -> Option<(String, String)> {
    let parsed: serde_json::Value = serde_json::from_slice(body).ok()?;
    let code = parsed.get("code")?.as_str()?;
    let message = parsed.get("message")?.as_str()?;
    parsed
        .is_object()
        .then(|| (code.to_owned(), message.to_owned()))
}

/// The `reason` of a reset frame's data.
fn reset_reason(data: &str) -> String {
    serde_json::from_str::<serde_json::Value>(data)
        .ok()
        .and_then(|value| value.get("reason")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_urls_are_checked() {
        for ok in [
            "https://stream.example.test",
            "HTTPS://stream.example.test:8443/base",
            "http://localhost:8080",
            "http://127.0.0.1",
            "http://127.8.9.1:9000",
            "http://[::1]:9000",
        ] {
            assert_eq!(check_stream_url(ok), Ok(()), "{ok}");
        }
        for (bad, why) in [
            ("stream.example.test", "it must start with https://"),
            ("ftp://stream.example.test", "it must start with https://"),
            (
                "http://stream.example.test",
                "plain http is only for this machine; use https://",
            ),
            (
                "http://127.0.0.1.example.test",
                "plain http is only for this machine; use https://",
            ),
            (
                "https://user:secret@stream.example.test",
                "it must not carry a user name or password",
            ),
            (
                "https://stream.example.test?key=1",
                "it must not carry a query or a fragment",
            ),
            (
                "https://stream.example.test/#top",
                "it must not carry a query or a fragment",
            ),
            ("https://", "it has no host"),
            ("https://[]", "it has no host"),
            ("https://[::1", "the address is not valid"),
            ("https://stream.example.test:port", "the port is not valid"),
        ] {
            assert_eq!(check_stream_url(bad), Err(why.to_owned()), "{bad}");
        }
        assert_eq!(
            origin("https://api.test:8443/v1?x#y").as_deref(),
            Some("https://api.test:8443")
        );
        assert_eq!(origin("api.test"), None);
        assert_eq!(origin("https://"), None);
    }

    #[test]
    fn api_errors_and_reset_reasons_are_read_leniently() {
        assert_eq!(
            api_error(br#"{"code":"unauthorized","message":"No."}"#),
            Some(("unauthorized".to_owned(), "No.".to_owned()))
        );
        for not_api in [
            &b"<html>"[..],
            b"null",
            b"[]",
            br#"{"code":7,"message":"x"}"#,
            br#"{"code":"x"}"#,
            b"",
        ] {
            assert_eq!(api_error(not_api), None);
        }
        assert_eq!(
            reset_reason(r#"{"reason":"cursor_expired"}"#),
            "cursor_expired"
        );
        for unknown in ["nope", "null", r#"{"reason":5}"#, "{}"] {
            assert_eq!(reset_reason(unknown), "unknown");
        }
        assert!(retries(408) && retries(429) && retries(503) && !retries(404) && !retries(302));
    }

    #[test]
    fn errors_and_requests_print_without_the_key() {
        let error = StreamError {
            kind: StreamErrorKind::Forbidden,
            status: Some(403),
            code: Some("organization_suspended".to_owned()),
            message: "Suspended.".to_owned(),
            retry_after: None,
        };
        assert_eq!(
            error.to_string(),
            "Suspended. (forbidden, 403 organization_suspended)"
        );
        let gave_up = StreamError {
            kind: StreamErrorKind::GaveUp,
            status: None,
            code: None,
            message: "down".to_owned(),
            retry_after: None,
        };
        assert_eq!(gave_up.to_string(), "down (gave_up)");
        let request = ConnectRequest {
            url: "https://stream.example.test/v1/events/stream".to_owned(),
            headers: vec![(
                "Authorization".to_owned(),
                "Bearer wu_live_secret".to_owned(),
            )],
        };
        assert_eq!(
            request.header("authorization"),
            Some("Bearer wu_live_secret")
        );
        let printed = format!("{request:?}");
        assert!(
            !printed.contains("wu_live_secret") && printed.contains("<redacted>"),
            "{printed}"
        );
    }
}
