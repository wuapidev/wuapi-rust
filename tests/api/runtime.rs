// Copied by wuapi-codegen from packages/sdk-codegen/templates/rust/tests/api/runtime.rs. Do not edit here.

//! The runtime against a local mock API: headers, retries, timeouts,
//! idempotency keys, errors and pagination.

use std::time::Duration;

use serde_json::{Value, json};

use crate::sdk::pagination::TryStreamExt;
use crate::sdk::{self, CursorPage, Error, Method, Paginator, RequestParts};
use crate::support::{KEY, MockApi, Reply, pairs};

fn get(path: &str) -> RequestParts {
    RequestParts::new(Method::Get, path).retryable()
}

#[tokio::test]
async fn sends_auth_and_json_headers() {
    let api = MockApi::start(vec![Reply::json(200, json!({"ok": true}))]).await;
    let client = api.client();
    let parts = RequestParts::new(Method::Post, "/v1/things")
        .query("dryRun", true)
        .query_opt("skipped", None::<&str>)
        .query_opt("q", Some("são paulo&x"))
        .body(&json!({"name": "a"}));
    let result: Value = client.http().request(parts).await.unwrap();
    assert_eq!(result, json!({"ok": true}));

    let calls = api.calls().await;
    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    assert_eq!(call.method, "POST");
    assert_eq!(call.path, "/v1/things");
    assert_eq!(
        call.query,
        pairs(&[("dryRun", "true"), ("q", "são paulo&x")])
    );
    assert_eq!(
        call.header("authorization"),
        Some(format!("Bearer {KEY}").as_str())
    );
    assert_eq!(call.header("accept"), Some("application/json"));
    assert_eq!(call.header("content-type"), Some("application/json"));
    assert_eq!(call.header("user-agent"), Some(sdk::USER_AGENT));
    assert_eq!(call.header(sdk::PROJECT_HEADER), None);
    assert_eq!(call.header(sdk::IDEMPOTENCY_HEADER), None);
    assert_eq!(call.body, Some(json!({"name": "a"})));
}

#[tokio::test]
async fn a_request_without_a_body_sends_no_content_type() {
    let api = MockApi::start(vec![Reply::json(200, json!({}))]).await;
    let _: Value = api
        .client()
        .http()
        .request(get("/v1/things"))
        .await
        .unwrap();
    let calls = api.calls().await;
    assert_eq!(calls[0].header("content-type"), None);
    assert_eq!(calls[0].body, None);
}

#[tokio::test]
async fn scopes_requests_to_a_project() {
    let api = MockApi::start(vec![
        Reply::json(200, json!({})),
        Reply::json(200, json!({})),
        Reply::json(200, json!({})),
    ])
    .await;
    let client = api.builder().project(" ext:customer_1 ").build().unwrap();
    assert_eq!(client.project(), Some("ext:customer_1"));
    assert_eq!(client.base_url(), api.url());
    let _: Value = client.http().request(get("/a")).await.unwrap();

    let other = client.with_project("proj_2").unwrap();
    assert_eq!(other.project(), Some("proj_2"));
    let _: Value = other.http().request(get("/b")).await.unwrap();

    let unscoped = api.client();
    assert_eq!(unscoped.project(), None);
    let _: Value = unscoped.http().request(get("/c")).await.unwrap();

    let calls = api.calls().await;
    assert_eq!(calls[0].header(sdk::PROJECT_HEADER), Some("ext:customer_1"));
    assert_eq!(calls[1].header(sdk::PROJECT_HEADER), Some("proj_2"));
    assert_eq!(calls[2].header(sdk::PROJECT_HEADER), None);

    assert!(matches!(client.with_project("  "), Err(Error::Config(_))));
    let long = "x".repeat(201);
    assert!(matches!(
        api.builder().project(long).build(),
        Err(Error::Config(_))
    ));
}

#[tokio::test]
async fn trims_the_base_url_and_needs_an_api_key() {
    let api = MockApi::start(vec![Reply::json(200, json!({}))]).await;
    let client = api
        .builder()
        .base_url(format!("{}//", api.url()))
        .build()
        .unwrap();
    assert_eq!(client.base_url(), api.url());
    let _: Value = client.http().request(get("/v1/x")).await.unwrap();
    assert_eq!(api.calls().await[0].path, "/v1/x");

    let error = sdk::ClientBuilder::new().api_key("  ").build().unwrap_err();
    assert_eq!(error.code(), "configuration");
    assert!(error.to_string().contains(sdk::API_KEY_ENV), "{error}");
    assert_eq!(error.status(), None);
}

#[tokio::test]
async fn retries_a_safe_request_on_5xx() {
    let api = MockApi::start(vec![
        Reply::json(
            503,
            json!({"code": "engine_unavailable", "message": "Retry shortly."}),
        ),
        Reply::status(500),
        Reply::json(200, json!({"id": "a"})),
    ])
    .await;
    let result: Value = api
        .client()
        .http()
        .request(get("/v1/things/a"))
        .await
        .unwrap();
    assert_eq!(result, json!({"id": "a"}));
    assert_eq!(api.calls().await.len(), 3);
}

#[tokio::test]
async fn gives_up_after_max_retries() {
    let api = MockApi::start(vec![
        Reply::status(503),
        Reply::status(503),
        Reply::status(503),
    ])
    .await;
    let client = api.builder().max_retries(1).build().unwrap();
    let error = client
        .http()
        .request::<Value>(get("/v1/x"))
        .await
        .unwrap_err();
    assert_eq!(error.status(), Some(503));
    assert_eq!(error.code(), "http_503");
    assert_eq!(api.calls().await.len(), 2);
}

#[tokio::test]
async fn never_repeats_a_request_that_is_not_idempotent() {
    // A 5xx or a timeout may have done the work: a POST or PATCH without an
    // idempotency key is sent once.
    let api = MockApi::start(vec![Reply::status(502), Reply::json(200, json!({}))]).await;
    let parts = RequestParts::new(Method::Patch, "/v1/things/a").body(&json!({"name": "b"}));
    let error = api
        .client()
        .http()
        .request::<Value>(parts)
        .await
        .unwrap_err();
    assert_eq!(error.status(), Some(502));
    assert_eq!(api.calls().await.len(), 1);

    let slow = MockApi::start(vec![
        Reply::json(200, json!({})).delay(Duration::from_millis(400)),
        Reply::json(200, json!({})),
    ])
    .await;
    let parts = RequestParts::new(Method::Post, "/v1/things").body(&json!({}));
    let error = slow
        .client()
        .http()
        .request::<Value>(parts)
        .timeout(Duration::from_millis(50))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Timeout { .. }), "{error:?}");
    assert_eq!(slow.calls().await.len(), 1);
}

#[tokio::test]
async fn retries_a_keyed_request_with_the_same_key() {
    let api = MockApi::start(vec![
        Reply::status(503),
        Reply::json(200, json!({"id": "m1"})),
        Reply::status(503),
        Reply::json(200, json!({"id": "m2"})),
    ])
    .await;
    let client = api.client();
    let parts = || {
        RequestParts::new(Method::Post, "/v1/messages")
            .body(&json!({"text": "hi"}))
            .keyed()
    };

    let _: Value = client.http().request(parts()).await.unwrap();
    let _: Value = client
        .http()
        .request(parts())
        .idempotency_key("order-1042")
        .await
        .unwrap();

    let calls = api.calls().await;
    assert_eq!(calls.len(), 4);
    let random = calls[0].header(sdk::IDEMPOTENCY_HEADER).unwrap();
    assert!(!random.is_empty());
    assert_eq!(calls[1].header(sdk::IDEMPOTENCY_HEADER), Some(random));
    assert_eq!(calls[2].header(sdk::IDEMPOTENCY_HEADER), Some("order-1042"));
    assert_eq!(calls[3].header(sdk::IDEMPOTENCY_HEADER), Some("order-1042"));
    assert_ne!(random, "order-1042");
    assert_eq!(calls[1].body, Some(json!({"text": "hi"})));
}

#[tokio::test]
async fn retries_429_for_any_request_and_honors_retry_after() {
    let api = MockApi::start(vec![
        Reply::json(
            429,
            json!({"code": "rate_limited", "message": "Slow down."}),
        )
        .header("retry-after", "0"),
        Reply::json(200, json!({"ok": true})),
    ])
    .await;
    let parts = RequestParts::new(Method::Patch, "/v1/things/a").body(&json!({}));
    let result: Value = api.client().http().request(parts).await.unwrap();
    assert_eq!(result, json!({"ok": true}));
    assert_eq!(api.calls().await.len(), 2);
}

#[tokio::test]
async fn a_long_retry_after_fails_at_once() {
    let api = MockApi::start(vec![
        Reply::json(429, json!({"code": "rate_limited", "message": "Cooldown."}))
            .header("retry-after", "600")
            .header("x-request-id", "req_cooldown"),
        Reply::json(200, json!({})),
    ])
    .await;
    let error = api
        .client()
        .http()
        .request::<Value>(get("/v1/x"))
        .await
        .unwrap_err();
    assert_eq!(error.status(), Some(429));
    assert_eq!(error.code(), "rate_limited");
    assert_eq!(error.retry_after(), Some(Duration::from_secs(600)));
    assert_eq!(error.request_id(), Some("req_cooldown"));
    assert_eq!(api.calls().await.len(), 1);
}

#[tokio::test]
async fn does_not_retry_other_4xx() {
    let api = MockApi::start(vec![
        Reply::json(
            409,
            json!({"code": "account_not_ready", "message": "Not connected.", "details": {"status": "disconnected"}}),
        )
        .header("x-request-id", "req_abc"),
        Reply::json(200, json!({})),
    ])
    .await;
    let error = api
        .client()
        .http()
        .request::<Value>(get("/v1/x"))
        .await
        .unwrap_err();
    let Error::Api {
        status,
        code,
        message,
        details,
        request_id,
        retry_after,
    } = &error
    else {
        panic!("expected an API error, got {error:?}");
    };
    assert_eq!(*status, 409);
    assert_eq!(code, "account_not_ready");
    assert_eq!(message, "Not connected.");
    assert_eq!(details.as_ref().unwrap()["status"], json!("disconnected"));
    assert_eq!(request_id.as_deref(), Some("req_abc"));
    assert_eq!(*retry_after, None);
    assert_eq!(
        error.to_string(),
        "Not connected. (409 account_not_ready, request req_abc)"
    );
    assert_eq!(api.calls().await.len(), 1);
}

#[tokio::test]
async fn an_error_without_a_json_body_gets_a_generic_code() {
    let api = MockApi::start(vec![Reply::text(404, "<html>not found</html>")]).await;
    let error = api
        .client()
        .http()
        .request::<Value>(get("/v1/x"))
        .await
        .unwrap_err();
    assert_eq!(error.status(), Some(404));
    assert_eq!(error.code(), "http_404");
    assert_eq!(error.request_id(), None);
    assert_eq!(error.details(), None);
    assert_eq!(
        error.to_string(),
        "Request failed with status 404. (404 http_404)"
    );
}

#[tokio::test]
async fn times_out_each_attempt_and_retries_safe_requests() {
    let slow = || Reply::json(200, json!({})).delay(Duration::from_millis(400));
    let api = MockApi::start(vec![slow(), Reply::json(200, json!({"ok": true}))]).await;
    let client = api
        .builder()
        .timeout(Duration::from_millis(50))
        .build()
        .unwrap();
    let result: Value = client.http().request(get("/v1/x")).await.unwrap();
    assert_eq!(result, json!({"ok": true}));
    assert_eq!(api.calls().await.len(), 2);

    let api = MockApi::start(vec![slow(), slow()]).await;
    let client = api
        .builder()
        .timeout(Duration::from_millis(50))
        .retry(sdk::RetryPolicy::none())
        .build()
        .unwrap();
    let error = client
        .http()
        .request::<Value>(get("/v1/x"))
        .await
        .unwrap_err();
    let Error::Timeout { timeout } = &error else {
        panic!("expected a timeout, got {error:?}");
    };
    assert_eq!(*timeout, Duration::from_millis(50));
    assert_eq!(error.code(), "timeout");
    assert_eq!(error.to_string(), "request timed out after 50 ms");
}

#[tokio::test]
async fn retries_any_request_that_never_connected() {
    // Nothing listens here: the connection is refused, so nothing was sent
    // and even a request that is not idempotent may be tried again.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let client = sdk::ClientBuilder::new()
        .api_key(KEY)
        .base_url(url)
        .retry(sdk::RetryPolicy {
            max_retries: 1,
            initial_backoff: Duration::from_millis(1),
            ..sdk::RetryPolicy::default()
        })
        .build()
        .unwrap();
    let parts = RequestParts::new(Method::Post, "/v1/things").body(&json!({}));
    let error = client.http().request::<Value>(parts).await.unwrap_err();
    assert!(matches!(error, Error::Network(_)), "{error:?}");
    assert_eq!(error.code(), "network_error");
    assert!(std::error::Error::source(&error).is_some());
}

#[tokio::test]
async fn empty_and_unexpected_bodies() {
    let api = MockApi::start(vec![
        Reply::status(204),
        Reply::json(200, json!({"ignored": true})),
        Reply::json(200, json!({"id": 1})).header("x-request-id", "req_shape"),
    ])
    .await;
    let client = api.client();
    let delete = || {
        RequestParts::new(Method::Delete, "/v1/things/a")
            .retryable()
            .no_content()
    };
    client.http().request::<()>(delete()).await.unwrap();
    // A body on a response declared empty is ignored.
    client.http().request::<()>(delete()).await.unwrap();

    let error = client
        .http()
        .request::<Vec<String>>(get("/v1/things"))
        .await
        .unwrap_err();
    let Error::Decode {
        status, request_id, ..
    } = &error
    else {
        panic!("expected a decode error, got {error:?}");
    };
    assert_eq!(*status, 200);
    assert_eq!(request_id.as_deref(), Some("req_shape"));
    assert_eq!(error.code(), "invalid_response");
}

#[tokio::test]
async fn a_body_that_does_not_serialize_sends_nothing() {
    struct Broken;
    impl serde::Serialize for Broken {
        fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom("broken"))
        }
    }
    let api = MockApi::start(vec![]).await;
    let parts = RequestParts::new(Method::Post, "/v1/things").body(&Broken);
    let error = api
        .client()
        .http()
        .request::<Value>(parts)
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Encode(_)), "{error:?}");
    assert!(api.calls().await.is_empty());
}

#[derive(Debug, serde::Deserialize)]
struct Page {
    items: Vec<String>,
    #[serde(rename = "nextCursor")]
    next_cursor: Option<String>,
}

impl CursorPage for Page {
    type Item = String;

    fn next_cursor(&self) -> Option<&str> {
        self.next_cursor.as_deref()
    }

    fn into_items(self) -> Vec<String> {
        self.items
    }
}

fn page(items: &[&str], next: Option<&str>) -> Reply {
    Reply::json(
        200,
        json!({"object": "list", "items": items, "nextCursor": next}),
    )
}

fn list(api: &MockApi, cursor: Option<&str>) -> Paginator<Page> {
    let parts = get("/v1/things").query("limit", 2);
    Paginator::new(
        api.client().http().clone(),
        parts,
        "cursor",
        cursor.map(str::to_owned),
    )
}

#[tokio::test]
async fn streams_every_item_across_pages() {
    let api = MockApi::start(vec![
        page(&["a", "b"], Some("c2")),
        page(&[], Some("c3")),
        page(&["c"], None),
    ])
    .await;
    let mut items = list(&api, None).stream();
    let mut seen = Vec::new();
    while let Some(item) = items.try_next().await.unwrap() {
        seen.push(item);
    }
    assert_eq!(seen, ["a", "b", "c"]);
    let calls = api.calls().await;
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0].query, pairs(&[("limit", "2")]));
    assert_eq!(calls[1].query, pairs(&[("limit", "2"), ("cursor", "c2")]));
    assert_eq!(calls[2].query, pairs(&[("limit", "2"), ("cursor", "c3")]));
}

#[tokio::test]
async fn fetches_single_pages_and_stops_early() {
    let api = MockApi::start(vec![
        page(&["a"], Some("c2")),
        page(&["b"], None),
        page(&["c", "d"], Some("c9")),
        page(&["x", "y"], Some("c2")),
    ])
    .await;
    let first = list(&api, None).page().await.unwrap();
    assert_eq!(first.items, ["a"]);
    let second = list(&api, None)
        .page_at(first.next_cursor().unwrap())
        .await
        .unwrap();
    assert_eq!(second.items, ["b"]);
    // A list created with a cursor starts there.
    assert_eq!(
        list(&api, Some("c8")).to_vec_max(2).await.unwrap(),
        ["c", "d"]
    );
    // Taking fewer items than a page holds asks for no further page.
    assert_eq!(list(&api, None).to_vec_max(1).await.unwrap(), ["x"]);
    let calls = api.calls().await;
    assert_eq!(calls.len(), 4);
    assert_eq!(calls[1].query, pairs(&[("limit", "2"), ("cursor", "c2")]));
    assert_eq!(calls[2].query, pairs(&[("limit", "2"), ("cursor", "c8")]));
}

#[tokio::test]
async fn a_failed_page_ends_the_stream_with_its_error() {
    let api = MockApi::start(vec![
        page(&["a"], Some("c2")),
        Reply::json(403, json!({"code": "forbidden", "message": "No."})),
        page(&["never"], None),
    ])
    .await;
    let mut pages = list(&api, None).pages();
    assert_eq!(pages.try_next().await.unwrap().unwrap().items, ["a"]);
    assert_eq!(pages.try_next().await.unwrap_err().code(), "forbidden");
    let error = list(&api, None).to_vec().await;
    assert!(error.is_ok(), "the third reply is a last page");
    assert_eq!(api.calls().await.len(), 3);
}

#[test]
fn encodes_path_segments() {
    assert_eq!(sdk::encode_path("120363@g.us"), "120363%40g.us");
    assert_eq!(sdk::encode_path("+58 424/1"), "%2B58%20424%2F1");
}
