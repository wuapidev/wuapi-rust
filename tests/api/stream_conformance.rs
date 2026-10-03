// Copied by wuapi-codegen from packages/sdk-codegen/templates/rust/tests/api/stream_conformance.rs. Do not edit here.

//! The Streams conformance suite (`packages/sdk-codegen/streams/`): every
//! scenario of `tests/stream-conformance/` played against this runtime, with
//! a scripted server, tokio's paused clock and scripted random numbers. The
//! same files run in every SDK the generator emits; a failure here is a
//! runtime that breaks the contract, never a scenario to adjust.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::time::Instant;

use crate::sdk;
use crate::sdk::stream::{
    Answer, BoxFuture, ConnectRequest, EventStream, HttpStreamTransport, Refused, StreamBody,
    StreamError, StreamErrorKind, StreamItem, StreamLimits, StreamOptions, StreamRefusal,
    StreamSpec, StreamTiming, StreamTransport,
};
use crate::support::KEY;

const SUITE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/stream-conformance");
pub const STREAM_HOST: &str = "https://stream.example.test";

fn suite_bytes(path: &str) -> Vec<u8> {
    std::fs::read(format!("{SUITE}/{path}")).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn suite_json(path: &str) -> Value {
    serde_json::from_slice(&suite_bytes(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn millis(value: &Value) -> Duration {
    Duration::from_millis(value.as_u64().unwrap())
}

/// The stream the scenarios were computed for: `description.json`.
pub fn suite_spec() -> StreamSpec {
    let d = suite_json("description.json");
    let (t, l) = (&d["timing"], &d["limits"]);
    let kind = |name: &str| match name {
        "unauthorized" => StreamErrorKind::Unauthorized,
        "forbidden" => StreamErrorKind::Forbidden,
        "invalid_request" => StreamErrorKind::InvalidRequest,
        "not_found" => StreamErrorKind::NotFound,
        "refused" => StreamErrorKind::Refused,
        other => panic!("description.json: unknown error `{other}`"),
    };
    StreamSpec {
        base_url: STREAM_HOST.to_owned(),
        path: d["path"].as_str().unwrap().to_owned(),
        resume_header: d["resume_header"].as_str().unwrap().to_owned(),
        reset_event: d["reset_event"].as_str().unwrap().to_owned(),
        timing: StreamTiming {
            retry: millis(&t["retry"]),
            heartbeat: millis(&t["heartbeat"]),
            idle_timeout: millis(&t["idle_timeout"]),
            open_timeout: millis(&t["open_timeout"]),
            backoff_base: millis(&t["backoff_base"]),
            backoff_cap: millis(&t["backoff_cap"]),
            min_gap: millis(&t["min_gap"]),
            stable_after: millis(&t["stable_after"]),
            retry_max: millis(&t["retry_max"]),
            retry_after_max: millis(&t["retry_after_max"]),
            replay_window: millis(&t["replay_window"]),
        },
        limits: StreamLimits {
            connect_budget: l["connect_budget"].as_u64().unwrap() as usize,
            connect_budget_window: millis(&l["connect_budget_window"]),
            max_frame_bytes: l["max_frame_bytes"].as_u64().unwrap() as usize,
            dedupe_window: l["dedupe_window"].as_u64().unwrap() as usize,
        },
        refusals: d["refusals"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| StreamRefusal {
                status: r["status"].as_u64().unwrap() as u16,
                code: r["code"].as_str().map(str::to_owned),
                error: r["error"].as_str().map(kind),
                api_only: r["api_only"].as_bool().unwrap_or(false),
                wait: r.get("wait").map(millis),
            })
            .collect(),
    }
}

/// One step of a scripted body.
enum Step {
    Bytes(Vec<u8>),
    Wait(Duration),
}

fn steps(stream: &Value) -> VecDeque<Step> {
    let mut out = VecDeque::new();
    for step in stream.as_array().unwrap() {
        if let Some(wait) = step.get("wait_ms") {
            out.push_back(Step::Wait(millis(wait)));
        } else if let Some(text) = step["text"].as_str() {
            out.push_back(Step::Bytes(text.as_bytes().to_vec()));
        } else if let Some(hex) = step["hex"].as_str() {
            let bytes = (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                .collect();
            out.push_back(Step::Bytes(bytes));
        } else {
            let bytes = suite_bytes(step["file"].as_str().unwrap());
            match step["chunk_bytes"].as_u64() {
                Some(size) => {
                    for chunk in bytes.chunks(size as usize) {
                        out.push_back(Step::Bytes(chunk.to_vec()));
                    }
                }
                None => out.push_back(Step::Bytes(bytes)),
            }
        }
    }
    out
}

/// A scripted body: its steps, then how it ends.
struct Body {
    steps: VecDeque<Step>,
    end: String,
    /// A wait under way, kept so that a dropped `chunk` loses none of it.
    waiting_until: Option<Instant>,
}

impl StreamBody for Body {
    fn chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, String>> {
        Box::pin(async move {
            loop {
                if let Some(until) = self.waiting_until {
                    tokio::time::sleep_until(until).await;
                    self.waiting_until = None;
                }
                match self.steps.pop_front() {
                    Some(Step::Bytes(bytes)) => return Ok(Some(bytes)),
                    Some(Step::Wait(wait)) => self.waiting_until = Some(Instant::now() + wait),
                    None => break,
                }
            }
            match self.end.as_str() {
                "close" => Ok(None),
                "error" => Err("terminated".to_owned()),
                _ => std::future::pending().await,
            }
        })
    }
}

/// A connect the scripted server saw.
pub struct Seen {
    /// When, since the script started.
    pub at: Duration,
    pub request: ConnectRequest,
}

/// A transport that plays a script: connect number n gets `responses[n]`,
/// in the words of the scenario files. The request is the one the real
/// transport would send.
pub struct Script {
    real: HttpStreamTransport,
    responses: Vec<Value>,
    started: Instant,
    pub seen: Mutex<Vec<Seen>>,
}

impl Script {
    pub fn new(real: HttpStreamTransport, responses: Vec<Value>) -> Arc<Self> {
        Arc::new(Self {
            real,
            responses,
            started: Instant::now(),
            seen: Mutex::default(),
        })
    }

    /// How many connects were made.
    pub fn connects(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

impl StreamTransport for Script {
    fn connect(&self, last_event_id: Option<String>) -> BoxFuture<'static, Result<Answer, String>> {
        let mut seen = self.seen.lock().unwrap();
        seen.push(Seen {
            at: self.started.elapsed(),
            request: self.real.request(last_event_id.as_deref()),
        });
        let response = self.responses.get(seen.len() - 1).cloned();
        let number = seen.len();
        Box::pin(async move {
            let Some(response) = response else {
                return Err(format!("unexpected connect number {number}"));
            };
            if let Some(after) = response.get("after_ms") {
                tokio::time::sleep(millis(after)).await;
            }
            if response["unreachable"] == json!(true) {
                return Err("connection refused".to_owned());
            }
            if response["hang"] == json!(true) {
                return std::future::pending().await;
            }
            let header = |name: &str| response["headers"][name].as_str().map(str::to_owned);
            if let Some(stream) = response.get("stream") {
                return Ok(Answer::Stream(Box::new(Body {
                    steps: steps(stream),
                    end: response["end"].as_str().unwrap_or("hang").to_owned(),
                    waiting_until: None,
                })));
            }
            let body = match (
                response["body"]["file"].as_str(),
                response["body"]["text"].as_str(),
            ) {
                (Some(file), _) => suite_bytes(file),
                (None, Some(text)) => text.as_bytes().to_vec(),
                (None, None) => Vec::new(),
            };
            Ok(Answer::Refused(Refused {
                status: response["status"].as_u64().unwrap() as u16,
                content_type: header("content-type"),
                body,
                // Seconds only, as the scenarios send them.
                retry_after: header("retry-after")
                    .and_then(|v| v.parse::<f64>().ok())
                    .map(Duration::from_secs_f64),
            }))
        })
    }
}

/// An item in the words of the scenario files (CONTRACT.md, section 8).
fn reported(item: &Result<StreamItem, StreamError>) -> Value {
    match item {
        Ok(StreamItem::Open) => json!({"type": "open"}),
        Ok(StreamItem::Event(event)) => json!({
            "type": "event", "event": event.event, "id": event.id, "cursor": event.cursor, "data": event.data,
        }),
        Ok(StreamItem::Reset { reason }) => json!({"type": "reset", "reason": reason}),
        Ok(StreamItem::Skipped { reason, event }) => {
            json!({"type": "skipped", "reason": reason.as_str(), "event": event})
        }
        Ok(StreamItem::Reconnecting(r)) => {
            let mut out = json!({
                "type": "reconnecting", "reason": r.reason.as_str(),
                "delay_ms": r.delay.as_millis() as u64, "failures": r.failures,
            });
            if r.reason == sdk::stream::ReconnectReason::Refused {
                out["status"] = json!(r.status);
                out["code"] = json!(r.code);
            }
            out
        }
        Ok(other) => panic!("an item the suite does not know: {other:?}"),
        Err(error) => json!({
            "type": "error", "kind": error.kind.as_str(), "status": error.status, "code": error.code,
        }),
    }
}

/// Plays one scenario. `Err` says what the runtime did differently.
async fn play(scenario: &Value, spec: &StreamSpec) -> Result<(), String> {
    let started = Instant::now();
    let params = &scenario["params"];
    let list = |value: &Value| -> Vec<String> {
        value
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .map(|v| v.as_str().unwrap().to_owned())
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut builder = sdk::ClientBuilder::new()
        .api_key(KEY)
        .stream_base_url(STREAM_HOST);
    if let Some(project) = scenario["client"]["project"].as_str() {
        builder = builder.project(project);
    }
    let http = builder.build_http().unwrap();
    let filters = [
        ("types", list(&params["types"])),
        ("accounts", list(&params["accounts"])),
    ];
    let connections = scenario["connections"].as_array().unwrap();
    let script = Script::new(
        HttpStreamTransport::new(&http, spec, &filters).unwrap(),
        connections.iter().map(|c| c["response"].clone()).collect(),
    );

    let draws: Vec<f64> = scenario["random"].as_array().map_or_else(
        || vec![0.0],
        |draws| draws.iter().map(|d| d.as_f64().unwrap()).collect(),
    );
    let drawn = AtomicUsize::new(0);
    let mut options = StreamOptions::new()
        .jitter(move || draws[drawn.fetch_add(1, Ordering::Relaxed).min(draws.len() - 1)]);
    let o = &scenario["options"];
    if let Some(window) = o["dedupe_window"].as_u64() {
        options = options.dedupe_window(window as usize);
    }
    if let Some(bytes) = o["max_frame_bytes"].as_u64() {
        options = options.max_frame_bytes(bytes as usize);
    }
    if let Some(after) = o.get("give_up_after_ms") {
        options = options.give_up_after(millis(after));
    }
    let mut stream = EventStream::with_transport(
        script.clone(),
        spec.clone(),
        params["last_event_id"].as_str().map(str::to_owned),
        options,
    );

    let expected = scenario["expect"].as_array().unwrap();
    let mut got = Vec::new();
    for wanted in expected {
        let Some(item) = stream.next_item().await else {
            break;
        };
        let mut said = reported(&item);
        said["at_ms"] = json!(started.elapsed().as_millis() as u64);
        for optional in ["data", "at_ms"] {
            if wanted.get(optional).is_none() {
                said.as_object_mut().unwrap().remove(optional);
            }
        }
        got.push(said);
        if item.is_err() {
            break;
        }
    }
    if &got != expected {
        return Err(format!(
            "reported\n{}\ninstead of\n{}",
            serde_json::to_string_pretty(&got).unwrap(),
            serde_json::to_string_pretty(expected).unwrap()
        ));
    }

    // Cancelled (or failed for good), the stream ends, stays silent and
    // connects no more.
    stream.close();
    tokio::time::sleep(Duration::from_secs(120)).await;
    if let Some(more) = stream.next_item().await {
        return Err(format!("reported {more:?} after it was closed"));
    }
    if !stream.is_stopped() {
        return Err("is not stopped after it was closed".to_owned());
    }

    let seen = script.seen.lock().unwrap();
    if seen.len() != connections.len() {
        return Err(format!(
            "made {} connects instead of {}",
            seen.len(),
            connections.len()
        ));
    }
    for (n, (connection, seen)) in connections.iter().zip(seen.iter()).enumerate() {
        let request = &seen.request;
        let (url, query) = request.url.split_once('?').unwrap_or((&request.url, ""));
        let mut problems = Vec::new();
        let mut check = |what: &str, actual: Value, wanted: Value| {
            if actual != wanted {
                problems.push(format!("{what} is {actual} instead of {wanted}"));
            }
        };
        check(
            "the URL",
            json!(url),
            json!(format!("{STREAM_HOST}{}", spec.path)),
        );
        check(
            "authorization",
            json!(request.header("authorization")),
            json!(format!("Bearer {KEY}")),
        );
        check(
            "accept",
            json!(request.header("accept")),
            json!("text/event-stream"),
        );
        check(
            "the key in the URL",
            json!(request.url.contains(KEY)),
            json!(false),
        );
        if let Some(at) = connection.get("at_ms") {
            check("the time", json!(seen.at.as_millis() as u64), at.clone());
        }
        if let Some(headers) = connection["request"]["headers"].as_object() {
            for (name, value) in headers {
                let header = name.replace("{project_header}", sdk::PROJECT_HEADER);
                check(&header, json!(request.header(&header)), value.clone());
            }
        }
        if let Some(wanted) = connection["request"].get("query") {
            let actual: serde_json::Map<String, Value> = query
                .split('&')
                .filter(|pair| !pair.is_empty())
                .map(|pair| {
                    let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
                    (name.to_owned(), json!(value.replace("%2C", ",")))
                })
                .collect();
            check("the query", Value::Object(actual), wanted.clone());
        }
        if !problems.is_empty() {
            return Err(format!("connect {}: {}", n + 1, problems.join("; ")));
        }
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn every_scenario_holds() {
    let spec = suite_spec();
    let index = suite_json("index.json");
    let scenarios = index["scenarios"].as_array().unwrap();
    assert!(scenarios.len() > 30, "only {} scenarios", scenarios.len());
    let mut failures = Vec::new();
    for path in scenarios {
        let scenario = suite_json(path.as_str().unwrap());
        if let Err(why) = play(&scenario, &spec).await {
            failures.push(format!(
                "{}: {}\n  {why}",
                scenario["name"], scenario["summary"]
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} scenarios failed:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}
