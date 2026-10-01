// Copied by wuapi-codegen from packages/sdk-codegen/templates/rust/tests/api/support.rs. Do not edit here.

//! A local mock API: queued replies in, recorded requests out. Nothing here
//! ever reaches the network.

// Not every API's generated tests use every helper.
#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use crate::sdk;

/// The API key every test client uses.
pub const KEY: &str = "sk_test_123";

/// One queued reply.
#[derive(Clone, Debug)]
pub struct Reply {
    status: u16,
    body: Option<Value>,
    text: Option<String>,
    headers: Vec<(String, String)>,
    delay: Option<Duration>,
}

impl Reply {
    /// A reply with a status and no body.
    pub fn status(status: u16) -> Self {
        Self {
            status,
            body: None,
            text: None,
            headers: Vec::new(),
            delay: None,
        }
    }

    /// A reply with a JSON body.
    pub fn json(status: u16, body: Value) -> Self {
        Self {
            body: Some(body),
            ..Self::status(status)
        }
    }

    /// A reply with a body that is not JSON.
    pub fn text(status: u16, text: &str) -> Self {
        Self {
            text: Some(text.to_owned()),
            ..Self::status(status)
        }
    }

    /// Adds a response header.
    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    /// Answers only after `delay`.
    pub fn delay(mut self, delay: Duration) -> Self {
        self.delay = Some(delay);
        self
    }

    fn template(&self) -> ResponseTemplate {
        let mut template = ResponseTemplate::new(self.status);
        if let Some(body) = &self.body {
            template = template.set_body_json(body);
        }
        if let Some(text) = &self.text {
            template = template.set_body_string(text.clone());
        }
        for (name, value) in &self.headers {
            template = template.insert_header(name.as_str(), value.as_str());
        }
        if let Some(delay) = self.delay {
            template = template.set_delay(delay);
        }
        template
    }
}

/// Answers each request with the next queued reply.
struct Queue(Mutex<VecDeque<Reply>>);

impl Respond for Queue {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        let next = self.0.lock().unwrap().pop_front();
        match next {
            Some(reply) => reply.template(),
            None => ResponseTemplate::new(599).set_body_string("no reply queued"),
        }
    }
}

/// A request the mock API received.
#[derive(Clone, Debug)]
pub struct Call {
    pub method: String,
    /// The path as sent, still percent-encoded.
    pub path: String,
    /// The decoded query parameters, in order.
    pub query: Vec<(String, String)>,
    /// Header names are lowercase.
    pub headers: HashMap<String, String>,
    pub body: Option<Value>,
}

impl Call {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }
}

/// A mock API on a local port.
pub struct MockApi {
    server: MockServer,
}

impl MockApi {
    /// Starts a server that answers with `replies`, in order.
    pub async fn start(replies: Vec<Reply>) -> Self {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(Queue(Mutex::new(replies.into())))
            .mount(&server)
            .await;
        Self { server }
    }

    pub fn url(&self) -> String {
        self.server.uri()
    }

    /// A builder aimed at this server, with waits short enough for tests.
    pub fn builder(&self) -> sdk::ClientBuilder {
        sdk::ClientBuilder::new()
            .api_key(KEY)
            .base_url(self.url())
            .timeout(Duration::from_secs(5))
            .retry(sdk::RetryPolicy {
                max_retries: 2,
                initial_backoff: Duration::from_millis(2),
                max_backoff: Duration::from_millis(4),
                max_retry_after: Duration::from_secs(1),
            })
    }

    /// A client aimed at this server.
    pub fn client(&self) -> sdk::Client {
        self.builder().build().unwrap()
    }

    /// Every request received so far, in order.
    pub async fn calls(&self) -> Vec<Call> {
        let requests = self.server.received_requests().await.unwrap();
        requests
            .iter()
            .map(|request| Call {
                method: request.method.to_string(),
                path: request.url.path().to_owned(),
                query: request
                    .url
                    .query_pairs()
                    .map(|(name, value)| (name.into_owned(), value.into_owned()))
                    .collect(),
                headers: request
                    .headers
                    .iter()
                    .map(|(name, value)| {
                        (
                            name.as_str().to_ascii_lowercase(),
                            value.to_str().unwrap_or_default().to_owned(),
                        )
                    })
                    .collect(),
                body: if request.body.is_empty() {
                    None
                } else {
                    Some(serde_json::from_slice(&request.body).unwrap())
                },
            })
            .collect()
    }
}

/// Query pairs as owned strings, to compare with [`Call::query`].
pub fn pairs(query: &[(&str, &str)]) -> Vec<(String, String)> {
    query
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect()
}

/// Numbers compare by value (`10` equals `10.0`); everything else exactly.
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| same(a, b))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(key, a)| b.get(key).is_some_and(|b| same(a, b)))
        }
        _ => a == b,
    }
}

/// Asserts that `actual` serializes to `expected`.
#[track_caller]
pub fn assert_json<T: serde::Serialize>(actual: &T, expected: &Value) {
    let actual = serde_json::to_value(actual).unwrap();
    assert!(
        same(&actual, expected),
        "JSON differs\n  actual:   {actual}\n  expected: {expected}"
    );
}

/// Parses test data.
#[track_caller]
pub fn json(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}

/// Parses test data into a typed value.
#[track_caller]
pub fn from_json<T: serde::de::DeserializeOwned>(text: &str) -> T {
    serde_json::from_str(text).unwrap()
}
