#![allow(dead_code)]
//! Shared e2e harness: an in-process `jev-mock` and gateway on ephemeral
//! ports, configured through `AppConfig::from_lookup` (never process env),
//! plus assertions for the official TypeSafe response shapes (PLAN §2.5).

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{header, HeaderMap, Method, Request, StatusCode};
use hyper_util::{
    client::legacy::{connect::HttpConnector, Client},
    rt::TokioExecutor,
};
use jev_mock::{MockOptions, MockServer};
use jev_provider::{config::AppConfig, BoxError};
use serde_json::{json, Value};
use std::{collections::BTreeSet, net::SocketAddr};
use tokio::{net::TcpListener, sync::oneshot, task::JoinHandle};

/// Gateway key accepted by every gateway from `spawn_gateway`.
pub const GATEWAY_KEY: &str = "test-gateway-key";
/// Env var name and value of the upstream key providers send to the mock.
pub const PROVIDER_KEY_ENV: &str = "MOCK_API_KEY";
pub const PROVIDER_KEY: &str = "sk-mock";

/// Starts a mock upstream on an ephemeral port; call inside a Tokio runtime.
pub fn spawn_mock(opts: MockOptions) -> MockServer {
    jev_mock::spawn(opts)
}

/// Absolute URL of `path` on the mock, e.g. `mock_url(&mock, "/v1/responses")`.
pub fn mock_url(mock: &MockServer, path: &str) -> String {
    format!("http://{}{path}", mock.addr)
}

/// `PROVIDERS_JSON` entry for `api_type` (`responses-api` or `messages-api`)
/// pointed at the mock, with the scenario set per provider (§2.4).
pub fn mock_provider(mock: &MockServer, api_type: &str, scenario: &str) -> Value {
    let path = match api_type {
        "messages-api" => "/v1/messages",
        _ => "/v1/responses",
    };
    json!({
        "type": api_type,
        "url": mock_url(mock, path),
        "api_key_env": PROVIDER_KEY_ENV,
        "headers": {"x-mock-scenario": scenario}
    })
}

/// A running gateway; dropping it also stops the server.
pub struct Gateway {
    pub addr: SocketAddr,
    shutdown: oneshot::Sender<()>,
    handle: JoinHandle<Result<(), BoxError>>,
}

impl Gateway {
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    /// Resolves the shutdown future and waits for `serve` to return.
    pub async fn stop(self) -> Result<(), BoxError> {
        let _ = self.shutdown.send(());
        self.handle.await.expect("gateway task panicked")
    }
}

/// Starts the gateway on an ephemeral `127.0.0.1` port; call inside a Tokio
/// runtime. `vars` are the configuration (point `PROVIDERS_JSON` at the mock
/// with `mock_provider`); they override these defaults: `GATEWAY_API_KEYS` =
/// `GATEWAY_KEY`, `PROVIDER_KEY_ENV` = `PROVIDER_KEY`, `SHUTDOWN_GRACE_MS` = 1000.
pub fn spawn_gateway(vars: &[(&str, &str)]) -> Gateway {
    let defaults = [
        ("GATEWAY_API_KEYS", GATEWAY_KEY),
        (PROVIDER_KEY_ENV, PROVIDER_KEY),
        ("SHUTDOWN_GRACE_MS", "1000"),
    ];
    let lookup = |name: &str| {
        vars.iter()
            .rev()
            .chain(defaults.iter())
            .find(|(k, _)| *k == name)
            .map(|(_, v)| (*v).to_owned())
    };
    let config = AppConfig::from_lookup(&lookup).expect("gateway config");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind gateway listener");
    listener
        .set_nonblocking(true)
        .expect("set gateway listener non-blocking");
    let addr = listener.local_addr().expect("gateway listener address");
    let listener = TcpListener::from_std(listener).expect("register gateway listener");
    let (shutdown, rx) = oneshot::channel::<()>();
    let handle = tokio::spawn(jev_provider::serve(listener, config, async {
        let _ = rx.await;
    }));
    Gateway {
        addr,
        shutdown,
        handle,
    }
}

/// A fully read gateway response.
#[derive(Debug)]
pub struct Resp {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl Resp {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| {
            panic!(
                "response body is not JSON ({e}): {}",
                String::from_utf8_lossy(&self.body)
            )
        })
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

/// Sends one request to the gateway with the `hyper-util` legacy client.
pub async fn request(
    gw: &Gateway,
    method: Method,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<Vec<u8>>,
) -> Resp {
    let client: Client<HttpConnector, Full<Bytes>> =
        Client::builder(TokioExecutor::new()).build_http();
    let mut builder = Request::builder().method(method).uri(gw.url(path));
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let req = builder
        .body(Full::new(Bytes::from(body.unwrap_or_default())))
        .expect("build request");
    let resp = client.request(req).await.expect("send request");
    let (parts, body) = resp.into_parts();
    let body = body.collect().await.expect("read body").to_bytes();
    Resp {
        status: parts.status,
        headers: parts.headers,
        body,
    }
}

/// `POST /v1/systemone` with the gateway key and a JSON body; `headers` are
/// added after (e.g. `Idempotency-Key`).
pub async fn post_systemone(gw: &Gateway, headers: &[(&str, &str)], body: &Value) -> Resp {
    let auth = format!("Bearer {GATEWAY_KEY}");
    let mut all = vec![
        (header::AUTHORIZATION.as_str(), auth.as_str()),
        (header::CONTENT_TYPE.as_str(), "application/json"),
    ];
    all.extend_from_slice(headers);
    let body = serde_json::to_vec(body).expect("serialize request");
    request(gw, Method::POST, "/v1/systemone", &all, Some(body)).await
}

/// Confidence recomputed from the 2-dp probabilities can differ from the
/// reported one, which the gateway computes before rounding (D23): 0.005 for
/// its own rounding plus up to 0.005 per probability, weighted by level
/// distance for Score. 0.05 covers the mock's distributions.
const CONFIDENCE_TOLERANCE: f64 = 0.05;

/// Asserts the official success shape for `request` and returns the body.
pub fn assert_official_success(resp: &Resp, request: &Value) -> Value {
    assert_eq!(
        resp.status,
        StatusCode::OK,
        "body: {}",
        String::from_utf8_lossy(&resp.body)
    );
    let request_id = resp
        .header("x-typesafe-request-id")
        .expect("x-typesafe-request-id header");
    assert!(!request_id.is_empty(), "empty x-typesafe-request-id");
    assert_eq!(
        resp.header("x-request-id"),
        Some(request_id),
        "x-request-id must equal x-typesafe-request-id"
    );

    let body = resp.json();
    assert_keys(&body, &["answers", "model", "usage"], "response");
    assert!(body["model"].is_string(), "model must be a string");
    assert_keys(&body["usage"], &["input_tokens", "output_tokens"], "usage");
    for key in ["input_tokens", "output_tokens"] {
        let v = &body["usage"][key];
        assert!(v.is_u64() || v.is_null(), "usage.{key} = {v}");
    }

    let questions = request["questions"]
        .as_object()
        .expect("request.questions object");
    let answers = body["answers"].as_object().expect("answers object");
    let answer_keys: BTreeSet<_> = answers.keys().collect();
    let question_keys: BTreeSet<_> = questions.keys().collect();
    assert_eq!(answer_keys, question_keys, "answers keys");

    for (key, question) in questions {
        let answer = &answers[key];
        let ctx = format!("answers.{key}");
        let mut numbers = answer.clone();
        numbers.as_object_mut().map(|o| o.remove("legend"));
        assert_two_decimals(&numbers, &ctx);
        match question["type"].as_str().expect("question type") {
            "noul" => {
                assert_keys(answer, &["noul", "type"], &ctx);
                assert_eq!(answer["type"], "noul", "{ctx}.type");
                assert!(answer["noul"].is_number(), "{ctx}.noul");
            }
            "choice" => {
                assert_keys(
                    answer,
                    &["choice", "confidence", "probabilities", "type"],
                    &ctx,
                );
                assert_eq!(answer["type"], "choice", "{ctx}.type");
                let criteria: Vec<&str> = question["criteria"]
                    .as_object()
                    .expect("choice criteria object")
                    .keys()
                    .map(String::as_str)
                    .collect();
                let choice = answer["choice"].as_str().expect("choice string");
                assert!(criteria.contains(&choice), "{ctx}.choice = {choice}");
                let probs = probabilities(answer, &criteria, &ctx);
                assert_confidence(answer, choice_confidence(&probs), &ctx);
            }
            "score" => {
                assert_keys(
                    answer,
                    &["confidence", "legend", "probabilities", "score", "type"],
                    &ctx,
                );
                assert_eq!(answer["type"], "score", "{ctx}.type");
                assert!(answer["score"].is_number(), "{ctx}.score");
                let n = question["criteria"]
                    .as_array()
                    .expect("score criteria array")
                    .len();
                let levels: Vec<String> = (0..n).map(|i| i.to_string()).collect();
                let levels: Vec<&str> = levels.iter().map(String::as_str).collect();
                assert_keys(&answer["legend"], &levels, &format!("{ctx}.legend"));
                let probs = probabilities(answer, &levels, &ctx);
                assert_confidence(answer, score_confidence(&probs), &ctx);
            }
            other => panic!("unknown question type {other}"),
        }
    }
    body
}

/// Asserts an official error: `status`, `x-typesafe-request-id`, and `detail`
/// equal to `Some(string)`, or for `None` an object with string `error_type`
/// and `message`. Returns `detail`.
pub fn assert_official_error(resp: &Resp, status: StatusCode, detail: Option<&str>) -> Value {
    assert_eq!(
        resp.status,
        status,
        "body: {}",
        String::from_utf8_lossy(&resp.body)
    );
    let request_id = resp
        .header("x-typesafe-request-id")
        .expect("x-typesafe-request-id header");
    assert!(!request_id.is_empty(), "empty x-typesafe-request-id");
    let body = resp.json();
    let got = body.get("detail").cloned().expect("detail member");
    match detail {
        Some(expected) => assert_eq!(got, json!(expected), "detail"),
        None => {
            assert!(got["error_type"].is_string(), "detail.error_type: {got}");
            assert!(got["message"].is_string(), "detail.message: {got}");
        }
    }
    got
}

/// Asserts `value` is an object whose keys are exactly `expected`.
fn assert_keys(value: &Value, expected: &[&str], ctx: &str) {
    let obj = value
        .as_object()
        .unwrap_or_else(|| panic!("{ctx} is not an object: {value}"));
    let got: BTreeSet<&str> = obj.keys().map(String::as_str).collect();
    let want: BTreeSet<&str> = expected.iter().copied().collect();
    assert_eq!(got, want, "{ctx} keys");
}

/// Returns `answer.probabilities` in `keys` order, normalized to sum 1.
fn probabilities(answer: &Value, keys: &[&str], ctx: &str) -> Vec<f64> {
    let ctx = format!("{ctx}.probabilities");
    assert_keys(&answer["probabilities"], keys, &ctx);
    let probs: Vec<f64> = keys
        .iter()
        .map(|k| {
            answer["probabilities"][*k]
                .as_f64()
                .unwrap_or_else(|| panic!("{ctx}.{k} is not a number"))
        })
        .collect();
    let sum: f64 = probs.iter().sum();
    assert!(sum > 0.0, "{ctx} sums to {sum}");
    probs.iter().map(|p| p / sum).collect()
}

/// Asserts every number in `value` has at most 2 decimal places (D23).
fn assert_two_decimals(value: &Value, ctx: &str) {
    match value {
        Value::Number(n) => {
            let x = n.as_f64().expect("finite number");
            assert!(
                ((x * 100.0).round() / 100.0 - x).abs() < 1e-9,
                "{ctx} = {x} has more than 2 decimals"
            );
        }
        Value::Object(o) => o
            .iter()
            .for_each(|(k, v)| assert_two_decimals(v, &format!("{ctx}.{k}"))),
        _ => {}
    }
}

fn assert_confidence(answer: &Value, expected: f64, ctx: &str) {
    let got = answer["confidence"]
        .as_f64()
        .unwrap_or_else(|| panic!("{ctx}.confidence is not a number"));
    assert!(
        (got - expected).abs() <= CONFIDENCE_TOLERANCE,
        "{ctx}.confidence = {got}, expected {expected}"
    );
}

/// §2.5 Choice: `(p_max − 1/n) / (1 − 1/n)`; `n ≤ 1` → 1.0.
fn choice_confidence(probs: &[f64]) -> f64 {
    let n = probs.len() as f64;
    if probs.len() <= 1 {
        return 1.0;
    }
    let p_max = probs.iter().copied().fold(0.0, f64::max);
    (p_max - 1.0 / n) / (1.0 - 1.0 / n)
}

/// §2.5 Score: `max(0, 1 − Σ pᵢ·|i − m| / MAD_unif)`, `m` the first most
/// likely level (D12); `n ≤ 1` → 1.0.
fn score_confidence(probs: &[f64]) -> f64 {
    let n = probs.len();
    if n <= 1 {
        return 1.0;
    }
    let m = (0..n).fold(0, |m, i| if probs[i] > probs[m] { i } else { m });
    let mad: f64 = (0..n).map(|i| probs[i] * i.abs_diff(m) as f64).sum();
    let center = (n - 1) as f64 / 2.0;
    let mad_unif = (0..n).map(|i| (i as f64 - center).abs()).sum::<f64>() / n as f64;
    (1.0 - mad / mad_unif).max(0.0)
}
