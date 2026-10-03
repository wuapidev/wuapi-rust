// Copied by wuapi-codegen from packages/sdk-codegen/templates/rust/tests/api/stream.rs. Do not edit here.

//! The stream runtime outside the conformance scenarios: the parser on its
//! own, where the stream connects to, the real transport against a local
//! mock server, and how a caller drives, observes and stops a stream.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::time::Instant;

use crate::sdk::pagination::StreamExt;
use crate::sdk::stream::{
    Answer, EventStream, HttpStreamTransport, ReconnectReason, SkipReason, SseItem, SseParser,
    StreamBudget, StreamErrorKind, StreamItem, StreamOptions, StreamSpec, StreamState,
    StreamTransport,
};
use crate::sdk::{self, Error};
use crate::stream_conformance::{STREAM_HOST, Script, suite_spec};
use crate::support::{KEY, MockApi, Reply, pairs};

const SSE: &str = "text/event-stream";

fn envelope(n: u32) -> String {
    format!(r#"{{"id":"evt_{n}","object":"event","type":"message.received"}}"#)
}

fn frame(n: u32, cursor: &str) -> String {
    format!(
        "id: {cursor}\nevent: message.received\ndata: {}\n\n",
        envelope(n)
    )
}

/// A scripted answer that is the stream: these texts, one chunk each.
fn body(texts: &[&str], end: &str) -> Value {
    let steps: Vec<Value> = texts.iter().map(|text| json!({"text": text})).collect();
    json!({"status": 200, "headers": {"content-type": SSE}, "stream": steps, "end": end})
}

/// A stream over a scripted server, on tokio's paused clock.
fn scripted(responses: Vec<Value>, options: StreamOptions) -> (Arc<Script>, EventStream) {
    let http = sdk::ClientBuilder::new()
        .api_key(KEY)
        .stream_base_url(STREAM_HOST)
        .build_http()
        .unwrap();
    let spec = suite_spec();
    let script = Script::new(
        HttpStreamTransport::new(&http, &spec, &[]).unwrap(),
        responses,
    );
    let stream = EventStream::with_transport(script.clone(), spec, None, options.jitter(|| 0.0));
    (script, stream)
}

/// The description with waits short enough for the real clock.
fn fast() -> StreamSpec {
    let mut spec = suite_spec();
    let ms = Duration::from_millis;
    spec.timing.retry = ms(1);
    spec.timing.min_gap = ms(1);
    spec.timing.backoff_base = ms(1);
    spec.timing.backoff_cap = ms(2);
    spec.timing.idle_timeout = ms(300);
    spec.timing.open_timeout = ms(2_000);
    spec
}

fn event_id(item: Option<Result<StreamItem, sdk::StreamError>>) -> String {
    match item {
        Some(Ok(StreamItem::Event(event))) => event.id.unwrap(),
        other => panic!("not an event: {other:?}"),
    }
}

mod parser {
    use super::*;

    fn parse(chunks: &[&[u8]]) -> Vec<SseItem> {
        let mut parser = SseParser::new(1024);
        chunks.iter().flat_map(|chunk| parser.feed(chunk)).collect()
    }

    fn one() -> SseItem {
        SseItem::Event {
            event: "message".to_owned(),
            data: "1".to_owned(),
            id: None,
        }
    }

    #[test]
    fn reports_events_cursors_retries_and_comments() {
        let items = parse(&[
            b": ping\nid: c1\n\n",
            b"retry: 7000\n\n",
            b"retry: 99999999999999999999999\n\n",
            b"id: c2\nevent: x\ndata: one\ndata: two\n\n",
        ]);
        assert_eq!(
            items,
            vec![
                SseItem::Comment,
                SseItem::Cursor("c1".to_owned()),
                SseItem::Retry(Duration::from_millis(7000)),
                SseItem::Retry(Duration::from_millis(u64::MAX)),
                SseItem::Event {
                    event: "x".to_owned(),
                    data: "one\ntwo".to_owned(),
                    id: Some("c2".to_owned()),
                },
            ]
        );
    }

    #[test]
    fn skips_one_byte_order_mark_and_only_a_whole_one_at_the_start() {
        assert_eq!(parse(&[&[0xEF, 0xBB, 0xBF], b"data: 1\n\n"]), vec![one()]);
        assert_eq!(
            parse(&[&[0xEF], &[0xBB], &[0xBF, b'd'], b"ata: 1\n\n"]),
            vec![one()]
        );
        // U+FB01 starts with the mark's first byte and is not one.
        assert_eq!(
            parse(&["\u{fb01}eld: x\ndata: 1\n\n".as_bytes()]),
            vec![one()]
        );
        assert_eq!(
            parse(&[b"data: 1\n\n", &[0xEF, 0xBB, 0xBF], b"data: 2\n\n"]),
            vec![one()]
        );
    }

    #[test]
    fn gives_the_same_result_for_every_split_of_the_bytes() {
        let text = format!(
            "retry: 10\r\n\r\n: c\r{}data: caf\u{e9} \u{20ac}\n\n",
            frame(1, "c1.a").replace('\n', "\r\n")
        );
        let whole = parse(&[text.as_bytes()]);
        assert_eq!(whole.len(), 4);
        for cut in 1..text.len() {
            let (head, tail) = text.as_bytes().split_at(cut);
            assert_eq!(parse(&[head, tail]), whole, "cut at {cut}");
        }
    }

    #[test]
    fn drops_a_line_or_a_frame_over_the_cap_and_keeps_its_id() {
        let mut parser = SseParser::new(16);
        let long = format!("id: c1\ndata: {}\n\n", "x".repeat(40));
        assert_eq!(
            parser.feed(long.as_bytes()),
            vec![SseItem::Oversized, SseItem::Cursor("c1".to_owned())]
        );
        assert_eq!(
            parser.feed(b"data: 12345\ndata: 67890\nevent: late\n\n"),
            vec![SseItem::Oversized]
        );
        assert_eq!(
            parser.feed(b"data: ok\n\n"),
            vec![SseItem::Event {
                event: "message".to_owned(),
                data: "ok".to_owned(),
                id: None,
            }]
        );
    }
}

#[test]
fn the_stream_is_served_where_the_key_may_go() {
    let spec = suite_spec();
    let url = |builder: sdk::ClientBuilder| {
        let http = builder.api_key(KEY).build_http().unwrap();
        HttpStreamTransport::new(&http, &spec, &[])
            .map(|transport| transport.url().to_owned())
            .map_err(Box::new)
    };
    let stream = |root: &str| format!("{root}{}", spec.path);

    // The stream's own host for the API's default URL.
    assert_eq!(url(sdk::ClientBuilder::new()).unwrap(), stream(STREAM_HOST));
    let default = format!("{}/", sdk::DEFAULT_BASE_URL);
    assert_eq!(
        url(sdk::ClientBuilder::new().base_url(default)).unwrap(),
        stream(STREAM_HOST)
    );
    // The API's own origin for any other API URL, so its key never reaches
    // the production stream.
    assert_eq!(
        url(sdk::ClientBuilder::new().base_url("https://api.staging.example.test/base")).unwrap(),
        stream("https://api.staging.example.test")
    );
    // The stream URL the client was given, before either.
    let given = sdk::ClientBuilder::new()
        .base_url("https://api.staging.example.test")
        .stream_base_url("http://127.0.0.1:9000/");
    assert_eq!(url(given).unwrap(), stream("http://127.0.0.1:9000"));

    for bad in [
        "http://stream.example.test",
        "https://user:secret@stream.example.test",
        "https://stream.example.test?key=1",
        "not a url",
    ] {
        let error = url(sdk::ClientBuilder::new().stream_base_url(bad)).unwrap_err();
        assert!(matches!(*error, Error::Config(_)), "{bad}: {error}");
        assert!(error.to_string().starts_with("the stream URL: "), "{error}");
    }
    let error = url(sdk::ClientBuilder::new().base_url("nowhere")).unwrap_err();
    assert_eq!(error.to_string(), "the API base URL has no usable origin");
}

#[test]
fn filters_are_comma_joined_and_empty_ones_left_out() {
    let http = sdk::ClientBuilder::new()
        .api_key(KEY)
        .project("proj_1")
        .build_http()
        .unwrap();
    let filters = [
        ("types", vec!["a".to_owned(), "b".to_owned()]),
        ("accounts", Vec::new()),
    ];
    let transport = HttpStreamTransport::new(&http, &suite_spec(), &filters).unwrap();
    assert_eq!(
        transport.url(),
        format!("{STREAM_HOST}/v1/events/stream?types=a%2Cb")
    );
    let request = transport.request(Some("c1.z"));
    assert_eq!(request.url, transport.url());
    assert_eq!(
        request.header("Authorization"),
        Some(format!("Bearer {KEY}").as_str())
    );
    assert_eq!(request.header(sdk::PROJECT_HEADER), Some("proj_1"));
    assert_eq!(request.header("Last-Event-ID"), Some("c1.z"));
    assert_eq!(transport.request(None).header("last-event-id"), None);
    assert_eq!(request.header("user-agent"), Some(sdk::USER_AGENT));
    let named = transport.clone().user_agent("inbox/1.2");
    assert_eq!(named.request(None).header("User-Agent"), Some("inbox/1.2"));
    for printed in [format!("{transport:?}"), format!("{request:?}")] {
        assert!(!printed.contains(KEY), "{printed}");
    }
}

#[tokio::test]
async fn reads_the_stream_over_http_and_resumes_with_its_cursor() {
    let api = MockApi::start(vec![
        Reply::raw(200, format!("retry: 1\n\n{}", frame(1, "c1.a")), SSE),
        Reply::raw(200, frame(2, "c1.b"), "Text/Event-Stream; charset=utf-8"),
    ])
    .await;
    let http = api.builder().project("proj_1").build_http().unwrap();
    let filters = [("types", vec!["a".to_owned(), "b".to_owned()])];
    let transport = HttpStreamTransport::new(&http, &fast(), &filters).unwrap();
    let mut stream = EventStream::with_transport(
        Arc::new(transport),
        fast(),
        Some("c1.0".to_owned()),
        StreamOptions::new(),
    );

    let first = stream.next_event().await.unwrap().unwrap();
    assert_eq!(first.event, "message.received");
    assert_eq!(first.id.as_deref(), Some("evt_1"));
    assert_eq!(first.cursor.as_deref(), Some("c1.a"));
    assert_eq!(first.data, envelope(1));
    assert_eq!(first.parse_as::<Value>().unwrap()["id"], "evt_1");
    // Typed as the API's webhook event: whether this envelope is one is the
    // API's business, and the stream is not affected either way.
    let _ = first.parse();
    let second = stream.next_event().await.unwrap().unwrap();
    assert_eq!(second.id.as_deref(), Some("evt_2"));
    assert_eq!(stream.last_event_id(), Some("c1.b"));
    stream.close();
    assert!(stream.next_event().await.is_none());

    let calls = api.calls().await;
    assert_eq!(calls.len(), 2);
    let call = &calls[0];
    assert_eq!(call.method, "GET");
    assert_eq!(call.path, "/v1/events/stream");
    assert_eq!(call.query, pairs(&[("types", "a,b")]));
    assert_eq!(
        call.header("authorization"),
        Some(format!("Bearer {KEY}").as_str())
    );
    assert_eq!(call.header("accept"), Some("text/event-stream"));
    assert_eq!(call.header("cache-control"), Some("no-cache"));
    assert_eq!(call.header("user-agent"), Some(sdk::USER_AGENT));
    assert_eq!(call.header(sdk::PROJECT_HEADER), Some("proj_1"));
    assert_eq!(call.header("last-event-id"), Some("c1.0"));
    assert_eq!(calls[1].header("last-event-id"), Some("c1.a"));
}

#[tokio::test]
async fn refusals_over_http_carry_the_status_the_body_and_retry_after() {
    let limit = json!({"code": "stream_connection_limit", "message": "Too many."});
    let api = MockApi::start(vec![
        Reply::json(429, limit).header("retry-after", "0.01"),
        Reply::text(503, &"x".repeat(100_000)),
        Reply::json(200, json!({})),
    ])
    .await;
    let transport = HttpStreamTransport::new(api.client().http(), &fast(), &[]).unwrap();

    // One connect on its own, as a diagnosis makes it.
    let Answer::Refused(refused) = transport.connect(None).await.unwrap() else {
        panic!("a 429 is not the stream");
    };
    assert_eq!(refused.status, 429);
    assert_eq!(refused.content_type.as_deref(), Some("application/json"));
    assert_eq!(refused.retry_after, Some(Duration::from_millis(10)));
    assert_eq!(
        serde_json::from_slice::<Value>(&refused.body).unwrap()["message"],
        "Too many."
    );
    let Answer::Refused(long) = transport.connect(None).await.unwrap() else {
        panic!("a 503 is not the stream");
    };
    assert_eq!((long.status, long.body.len()), (503, 64 * 1024));
    let answer = transport.connect(None).await.unwrap();
    assert!(format!("{answer:?}").starts_with("Refused"), "{answer:?}");

    // The same answers, through a stream.
    let api = MockApi::start(vec![
        Reply::json(
            429,
            json!({"code": "rate_limited", "message": "Slow down."}),
        )
        .header("retry-after", "0.01"),
        Reply::json(
            401,
            json!({"code": "unauthorized", "message": "Missing or invalid API key."}),
        ),
    ])
    .await;
    let transport = HttpStreamTransport::new(api.client().http(), &fast(), &[]).unwrap();
    let mut stream =
        EventStream::with_transport(Arc::new(transport), fast(), None, StreamOptions::new());
    let Some(Ok(StreamItem::Reconnecting(waiting))) = stream.next_item().await else {
        panic!("a 429 is retried");
    };
    assert_eq!(waiting.reason, ReconnectReason::Refused);
    assert_eq!(waiting.status, Some(429));
    assert_eq!(waiting.code.as_deref(), Some("rate_limited"));
    assert_eq!(waiting.message.as_deref(), Some("Slow down."));
    assert_eq!(waiting.retry_after, Some(Duration::from_millis(10)));
    assert_eq!(waiting.failures, 1);
    let error = stream.next_item().await.unwrap().unwrap_err();
    assert_eq!(error.kind, StreamErrorKind::Unauthorized);
    assert_eq!(error.status, Some(401));
    assert_eq!(error.code.as_deref(), Some("unauthorized"));
    assert_eq!(error.message, "Missing or invalid API key.");
    assert_eq!(
        error.to_string(),
        "Missing or invalid API key. (unauthorized, 401 unauthorized)"
    );
    assert!(stream.is_stopped());
    assert!(stream.next_item().await.is_none());
    assert_eq!(api.calls().await.len(), 2);
}

#[tokio::test]
async fn a_connect_that_gets_no_answer_says_so_without_the_url() {
    let http = sdk::ClientBuilder::new()
        .api_key(KEY)
        // Nothing listens on port 9: the connect is refused at once.
        .stream_base_url("http://127.0.0.1:9")
        .build_http()
        .unwrap();
    let transport = HttpStreamTransport::new(&http, &fast(), &[]).unwrap();
    let why = transport.connect(None).await.unwrap_err();
    assert!(!why.contains("127.0.0.1") && !why.contains(KEY), "{why}");

    let mut stream =
        EventStream::with_transport(Arc::new(transport), fast(), None, StreamOptions::new());
    let Some(Ok(StreamItem::Reconnecting(waiting))) = stream.next_item().await else {
        panic!("an unreachable host is retried");
    };
    assert_eq!(waiting.reason, ReconnectReason::Unreachable);
    assert!(waiting.message.is_some() && waiting.status.is_none());
}

#[tokio::test]
async fn a_connect_over_http_that_takes_too_long_is_retried() {
    let api = MockApi::start(vec![
        Reply::raw(200, ": ping\n\n", SSE).delay(Duration::from_secs(5)),
    ])
    .await;
    let mut spec = fast();
    spec.timing.open_timeout = Duration::from_millis(50);
    let transport = HttpStreamTransport::new(api.client().http(), &spec, &[]).unwrap();
    let mut stream =
        EventStream::with_transport(Arc::new(transport), spec, None, StreamOptions::new());
    // The answer does not come within the open timeout.
    let Some(Ok(StreamItem::Reconnecting(waiting))) = stream.next_item().await else {
        panic!("a slow connect is retried");
    };
    assert_eq!(waiting.reason, ReconnectReason::Unreachable);
    assert_eq!(
        waiting.message.as_deref(),
        Some("the connection took too long")
    );
}

#[tokio::test(start_paused = true)]
async fn next_event_skips_everything_else_and_tells_on_status() {
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let log = seen.clone();
    let options = StreamOptions::new().on_status(move |item| {
        let said = match item {
            StreamItem::Open => "open".to_owned(),
            StreamItem::Reset { reason } => format!("reset {reason}"),
            StreamItem::Skipped { reason, event } => {
                format!("skipped {} {event:?}", reason.as_str())
            }
            StreamItem::Reconnecting(r) => {
                format!("reconnecting {} {:?}", r.reason.as_str(), r.delay)
            }
            other => format!("{other:?}"),
        };
        log.lock().unwrap().push(said);
    });
    let reset = "event: reset\ndata: {\"reason\":\"cursor_expired\"}\n\n";
    let (script, mut stream) = scripted(
        vec![
            body(&[reset, "data: nope\n\n", &frame(1, "c1.a")], "close"),
            body(&[&frame(2, "c1.b")], "hang"),
        ],
        options,
    );
    assert_eq!(stream.state(), StreamState::Waiting);
    assert_eq!(stream.failures(), 0);
    assert!(stream.down_since().is_some());
    assert_eq!(
        stream.next_event().await.unwrap().unwrap().id.as_deref(),
        Some("evt_1")
    );
    assert_eq!(stream.state(), StreamState::Open);
    assert!(stream.down_since().is_none());
    assert_eq!(
        stream.next_event().await.unwrap().unwrap().id.as_deref(),
        Some("evt_2")
    );
    assert_eq!(
        *seen.lock().unwrap(),
        [
            "open",
            "reset cursor_expired",
            "skipped invalid_data Some(\"message\")",
            "reconnecting ended 3s",
            "open",
        ]
    );
    assert_eq!(stream.failures(), 1);
    assert_eq!(script.connects(), 2);
    assert!(format!("{stream:?}").contains("Open"), "{stream:?}");
    assert_eq!(SkipReason::Oversized.as_str(), "oversized");
}

#[tokio::test(start_paused = true)]
async fn a_stream_is_a_stream_of_events_or_of_items() {
    let unauthorized =
        json!({"status": 401, "body": {"text": r#"{"code":"unauthorized","message":"No."}"#}});
    let (_, stream) = scripted(
        vec![
            body(&[&frame(1, "c1.a"), &frame(2, "c1.b")], "close"),
            unauthorized.clone(),
        ],
        StreamOptions::new(),
    );
    let events: Vec<_> = stream.into_stream().collect().await;
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].as_ref().unwrap().id.as_deref(), Some("evt_1"));
    assert_eq!(events[1].as_ref().unwrap().id.as_deref(), Some("evt_2"));
    assert_eq!(
        events[2].as_ref().unwrap_err().kind,
        StreamErrorKind::Unauthorized
    );

    let (_, stream) = scripted(
        vec![body(&[&frame(1, "c1.a")], "close"), unauthorized],
        StreamOptions::new(),
    );
    let items: Vec<_> = stream.into_items().collect().await;
    assert_eq!(items.len(), 4);
    assert_eq!(items[0], Ok(StreamItem::Open));
    assert!(matches!(items[1], Ok(StreamItem::Event(_))));
    assert!(matches!(items[2], Ok(StreamItem::Reconnecting(_))));
    assert!(items[3].is_err());
}

#[tokio::test(start_paused = true)]
async fn next_item_loses_nothing_when_its_future_is_dropped() {
    let slow = json!({
        "status": 200, "headers": {"content-type": SSE}, "after_ms": 5000, "end": "hang",
        "stream": [{"text": frame(1, "c1.a")}, {"wait_ms": 5000}, {"text": frame(2, "c1.b")}],
    });
    let (script, mut stream) = scripted(vec![slow], StreamOptions::new());
    let mut got = Vec::new();
    // A caller that races the stream against a one-second tick.
    while got.len() < 3 {
        if let Ok(item) = tokio::time::timeout(Duration::from_secs(1), stream.next_item()).await {
            got.push(item.unwrap().unwrap());
        }
    }
    assert_eq!(got[0], StreamItem::Open);
    assert_eq!(event_id(Some(Ok(got[1].clone()))), "evt_1");
    assert_eq!(event_id(Some(Ok(got[2].clone()))), "evt_2");
    assert_eq!(script.connects(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_stopped_stream_starts_again_when_asked() {
    let unauthorized =
        json!({"status": 401, "body": {"text": r#"{"code":"unauthorized","message":"No."}"#}});
    let (script, mut stream) = scripted(
        vec![
            body(&[&frame(1, "c1.a")], "close"),
            unauthorized,
            body(&[&frame(2, "c1.b")], "hang"),
            body(&[&frame(3, "c1.c")], "hang"),
        ],
        StreamOptions::new(),
    );
    let started = Instant::now();
    assert_eq!(stream.next_item().await, Some(Ok(StreamItem::Open)));
    assert_eq!(event_id(stream.next_item().await), "evt_1");
    assert!(matches!(
        stream.next_item().await,
        Some(Ok(StreamItem::Reconnecting(_)))
    ));
    assert!(stream.next_item().await.unwrap().is_err());
    assert!(stream.is_stopped() && stream.state() == StreamState::Stopped);
    assert_eq!(stream.last_event_id(), Some("c1.a"));

    // Again in ten minutes, from now: the cursor is dropped.
    stream.restart_after(Duration::from_secs(600), false);
    assert_eq!(stream.state(), StreamState::Waiting);
    assert_eq!(stream.next_item().await, Some(Ok(StreamItem::Open)));
    assert_eq!(event_id(stream.next_item().await), "evt_2");
    {
        let seen = script.seen.lock().unwrap();
        assert_eq!(seen[2].at, started.elapsed());
        assert_eq!(seen[2].at, Duration::from_secs(603));
        assert_eq!(seen[1].request.header("last-event-id"), Some("c1.a"));
        assert_eq!(seen[2].request.header("last-event-id"), None);
    }

    // An open stream can be restarted too, keeping its cursor or not.
    stream.restart_after(Duration::ZERO, true);
    assert!(stream.down_since().is_some());
    assert_eq!(stream.next_item().await, Some(Ok(StreamItem::Open)));
    assert_eq!(
        script.seen.lock().unwrap()[3]
            .request
            .header("last-event-id"),
        Some("c1.b")
    );
    stream.clear_cursor();
    assert_eq!(stream.last_event_id(), None);
}

#[tokio::test(start_paused = true)]
async fn every_stream_of_one_client_draws_on_one_budget() {
    let http = sdk::ClientBuilder::new()
        .api_key(KEY)
        .stream_base_url(STREAM_HOST)
        .build_http()
        .unwrap();
    let spec = suite_spec();
    let budget = StreamBudget::of(&http, &spec.limits);
    // A project-scoped copy of the client draws on the same budget.
    let scoped = StreamBudget::of(&http.with_project("proj_1").unwrap(), &spec.limits);
    let started = Instant::now();
    let open: Vec<Value> = (0..7).map(|_| body(&[], "hang")).collect();
    let script = Script::new(HttpStreamTransport::new(&http, &spec, &[]).unwrap(), open);
    let mut streams: Vec<EventStream> = (0..7)
        .map(|n| {
            let budget = if n % 2 == 0 {
                budget.clone()
            } else {
                scoped.clone()
            };
            // The budget the stream was given wins over the one offered later.
            let options = StreamOptions::new()
                .budget(budget)
                .or_budget(StreamBudget::new(&spec.limits));
            EventStream::with_transport(script.clone(), spec.clone(), None, options)
        })
        .collect();
    // Six may connect at once; the seventh waits for the window to pass.
    assert_eq!(budget.next_slot(started), started + Duration::from_secs(60));
    for stream in &mut streams {
        assert_eq!(stream.next_item().await, Some(Ok(StreamItem::Open)));
    }
    let seen = script.seen.lock().unwrap();
    let at: Vec<u64> = seen.iter().map(|s| s.at.as_secs()).collect();
    assert_eq!(at, [0, 0, 0, 0, 0, 0, 60]);

    // A budget of its own is not held back.
    let own = StreamBudget::new(&spec.limits);
    assert_eq!(own.next_slot(Instant::now()), Instant::now());
}

#[tokio::test(start_paused = true)]
async fn the_first_connect_can_be_put_off_and_ids_leave_the_dedupe_window() {
    let frames = [
        frame(1, "a"),
        frame(2, "b"),
        frame(1, "c"),
        frame(2, "d"),
        frame(2, "e"),
        frame(3, "f"),
    ];
    let texts: Vec<&str> = frames.iter().map(String::as_str).collect();
    let options = StreamOptions::new()
        .start_after(Duration::from_secs(30))
        .dedupe_window(1);
    assert!(format!("{options:?}").contains("start_after: Some(30s)"));
    let (script, mut stream) = scripted(vec![body(&texts, "hang")], options);
    assert_eq!(stream.next_item().await, Some(Ok(StreamItem::Open)));
    assert_eq!(script.seen.lock().unwrap()[0].at, Duration::from_secs(30));
    // 1 again is delivered (2 pushed it out); 2 right after 2 is not.
    let mut ids = Vec::new();
    for _ in 0..5 {
        ids.push(event_id(stream.next_item().await));
    }
    assert_eq!(ids, ["evt_1", "evt_2", "evt_1", "evt_2", "evt_3"]);
    assert_eq!(stream.last_event_id(), Some("f"));
}
