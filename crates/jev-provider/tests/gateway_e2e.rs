//! Gateway behavior with both provider types behind `jev-mock`: mixed routes,
//! `/v1/models`, upstream headers, auth, 404/405, body limits, slow-client
//! timeouts, success-body key order (request order), idempotency and admission.

mod common;

use common::{
    assert_official_error, assert_official_success, mock_provider, post_systemone, request,
    spawn_gateway, spawn_mock, Gateway, Resp, GATEWAY_KEY,
};
use hyper::{header, Method, StatusCode};
use jev_mock::{MockOptions, MockServer};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

/// Upstream names that must never reach a caller (D20).
const RESPONSES_PROVIDER: &str = "upstream-oai";
const MESSAGES_PROVIDER: &str = "upstream-anth";
const RESPONSES_MODEL: &str = "gpt-mock-upstream";
const MESSAGES_MODEL: &str = "claude-mock-upstream";

/// `examples/request.json` addressed to `model`.
fn example(model: &str) -> Value {
    let mut request: Value =
        serde_json::from_str(include_str!("../../../examples/request.json")).expect("example");
    request["model"] = json!(model);
    request
}

/// Mixed-provider config: `jev-openai` → responses-api and `jev-latest` →
/// messages-api, both with scenario `ok` and a versioned `response_model`.
fn mixed_config(mock: &MockServer) -> (String, String) {
    let providers = json!({
        RESPONSES_PROVIDER: mock_provider(mock, "responses-api", "ok"),
        MESSAGES_PROVIDER: mock_provider(mock, "messages-api", "ok"),
    })
    .to_string();
    let routes = json!({
        "jev-openai": {
            "provider": RESPONSES_PROVIDER, "model": RESPONSES_MODEL,
            "response_model": "jev-openai-2026-10-01",
            "description": "Responses-backed", "release_date": "2026-10-01"
        },
        "jev-latest": {
            "provider": MESSAGES_PROVIDER, "model": MESSAGES_MODEL,
            "response_model": "jev-latest-2026-10-13"
        }
    })
    .to_string();
    (providers, routes)
}

fn spawn_mixed(mock: &MockServer, extra: &[(&str, &str)]) -> Gateway {
    let (providers, routes) = mixed_config(mock);
    let mut vars = vec![
        ("PROVIDERS_JSON", providers.as_str()),
        ("MODEL_ROUTES_JSON", routes.as_str()),
    ];
    vars.extend_from_slice(extra);
    spawn_gateway(&vars)
}

fn upstream_calls(mock: &MockServer, path: &str) -> usize {
    mock.requests().iter().filter(|r| r["path"] == path).count()
}

#[tokio::test]
async fn mixed_routes_both_succeed_with_response_mode_and_model() {
    let mock = spawn_mock(MockOptions::default());
    let gw = spawn_mixed(&mock, &[]);

    for (alias, mode, model, path) in [
        (
            "jev-openai",
            "responses-adapted",
            "jev-openai-2026-10-01",
            "/v1/responses",
        ),
        (
            "jev-latest",
            "messages-adapted",
            "jev-latest-2026-10-13",
            "/v1/messages",
        ),
    ] {
        let request = example(alias);
        let resp = post_systemone(&gw, &[], &request).await;
        let body = assert_official_success(&resp, &request);
        assert_eq!(resp.header("x-system-one-response-mode"), Some(mode));
        assert_eq!(body["model"], model, "{alias}");
        assert_eq!(upstream_calls(&mock, path), 1, "{alias}");
    }
}

#[tokio::test]
async fn models_lists_public_aliases_only_and_needs_the_key() {
    let mock = spawn_mock(MockOptions::default());
    let gw = spawn_mixed(&mock, &[]);
    let auth = format!("Bearer {GATEWAY_KEY}");

    let resp = request(
        &gw,
        Method::GET,
        "/v1/models",
        &[(header::AUTHORIZATION.as_str(), &auth)],
        None,
    )
    .await;
    assert_eq!(resp.status, StatusCode::OK);
    assert!(resp
        .header("x-typesafe-request-id")
        .is_some_and(|v| !v.is_empty()));
    let body = resp.json();
    let models = body["models"].as_array().expect("models array");
    assert_eq!(body.as_object().map(|o| o.len()), Some(1), "{body}");
    let mut names = Vec::new();
    for model in models {
        let entry = model.as_object().expect("model object");
        let mut keys: Vec<&str> = entry.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["description", "name", "release_date"]);
        assert!(entry.values().all(Value::is_string), "{model}");
        let date = model["release_date"].as_str().unwrap();
        assert!(is_iso_date_shape(date), "release_date {date}");
        names.push(model["name"].as_str().unwrap());
    }
    names.sort_unstable();
    assert_eq!(names, ["jev-latest", "jev-openai"]);
    assert_eq!(models[1]["description"], "Responses-backed");
    assert_eq!(models[1]["release_date"], "2026-10-01");

    let text = String::from_utf8_lossy(&resp.body);
    for private in [
        RESPONSES_PROVIDER,
        MESSAGES_PROVIDER,
        RESPONSES_MODEL,
        MESSAGES_MODEL,
    ] {
        assert!(
            !text.contains(private),
            "/v1/models leaks {private}: {text}"
        );
    }

    let missing = request(&gw, Method::GET, "/v1/models", &[], None).await;
    let detail = assert_official_error(&missing, StatusCode::FORBIDDEN, None);
    assert_eq!(detail["error_type"], "authentication_error");
    assert_eq!(detail["message"], MISSING_KEY);
}

/// `^\d{4}-\d{2}-\d{2}$` without a regex dependency.
fn is_iso_date_shape(s: &str) -> bool {
    s.len() == 10
        && s.bytes().enumerate().all(|(i, b)| match i {
            4 | 7 => b == b'-',
            _ => b.is_ascii_digit(),
        })
}

#[tokio::test]
async fn upstream_headers_are_hidden_by_default_and_exposed_on_opt_in() {
    let mock = spawn_mock(MockOptions::default());
    let request = example("jev-openai");

    let gw = spawn_mixed(&mock, &[]);
    let resp = post_systemone(&gw, &[], &request).await;
    assert_official_success(&resp, &request);
    for name in [
        "x-upstream-provider",
        "x-upstream-model",
        "x-upstream-request-id",
    ] {
        assert!(resp.header(name).is_none(), "{name} sent by default");
    }

    mock.clear();
    let gw = spawn_mixed(&mock, &[("EXPOSE_UPSTREAM_HEADERS", "true")]);
    let resp = post_systemone(&gw, &[], &request).await;
    assert_official_success(&resp, &request);
    let captured = mock.requests();
    assert_eq!(captured.len(), 1);
    let seq = captured[0]["seq"].as_u64().expect("captured seq");
    assert_eq!(
        resp.header("x-upstream-request-id"),
        Some(format!("req_mock_{seq}").as_str())
    );
    assert_eq!(resp.header("x-upstream-provider"), Some(RESPONSES_PROVIDER));
    assert_eq!(resp.header("x-upstream-model"), Some(RESPONSES_MODEL));
}

/// Official TypeSafe auth messages (8B probe P1).
const MISSING_KEY: &str = "Must supply an API key! Check your request and try again.";
const INVALID_KEY: &str =
    "Cannot authenticate with the server. Please check your API key and try again.";

#[tokio::test]
async fn auth_without_a_bearer_token_is_403_and_unknown_token_is_401() {
    let mock = spawn_mock(MockOptions::default());
    let gw = spawn_mixed(&mock, &[]);
    let body = serde_json::to_vec(&example("jev-openai")).unwrap();
    let json_type = (header::CONTENT_TYPE.as_str(), "application/json");

    for (authorization, status, message) in [
        (None, StatusCode::FORBIDDEN, MISSING_KEY),
        (Some("Basic x"), StatusCode::FORBIDDEN, MISSING_KEY),
        (Some("Bearer "), StatusCode::FORBIDDEN, MISSING_KEY),
        (Some("BEARER wrong"), StatusCode::UNAUTHORIZED, INVALID_KEY),
        (
            Some("Bearer not-the-key"),
            StatusCode::UNAUTHORIZED,
            INVALID_KEY,
        ),
    ] {
        let mut headers = vec![json_type];
        headers.extend(authorization.map(|v| (header::AUTHORIZATION.as_str(), v)));
        let resp = request(
            &gw,
            Method::POST,
            "/v1/systemone",
            &headers,
            Some(body.clone()),
        )
        .await;
        let detail = assert_official_error(&resp, status, None);
        assert_eq!(
            detail["error_type"], "authentication_error",
            "{authorization:?}"
        );
        assert_eq!(detail["message"], message, "{authorization:?}");
        let full = resp.json();
        assert_eq!(full["error"]["code"], "invalid_api_key");
        assert_eq!(full["error"]["message"], message);
        assert_eq!(full["request_id"], resp.header("x-request-id").unwrap());
    }
    assert!(mock.requests().is_empty(), "no upstream call without auth");

    // The scheme name is case-insensitive (RFC 7235).
    let lowercase = format!("bearer {GATEWAY_KEY}");
    let resp = request(
        &gw,
        Method::POST,
        "/v1/systemone",
        &[json_type, (header::AUTHORIZATION.as_str(), &lowercase)],
        Some(body),
    )
    .await;
    assert_official_success(&resp, &example("jev-openai"));
}

#[tokio::test]
async fn wrong_method_is_405_and_unknown_path_is_404() {
    let mock = spawn_mock(MockOptions::default());
    let gw = spawn_mixed(&mock, &[]);

    let resp = request(&gw, Method::GET, "/v1/systemone", &[], None).await;
    assert_official_error(
        &resp,
        StatusCode::METHOD_NOT_ALLOWED,
        Some("Method Not Allowed"),
    );
    assert_eq!(resp.header("allow"), Some("POST"));

    let resp = request(&gw, Method::GET, "/v1/nothing-here", &[], None).await;
    assert_official_error(&resp, StatusCode::NOT_FOUND, Some("Not Found"));
}

#[tokio::test]
async fn openapi_is_served_without_a_token_and_documents_live_routes() {
    let mock = spawn_mock(MockOptions::default());
    // The default test config sets GATEWAY_API_KEYS.
    let gw = spawn_mixed(&mock, &[]);

    let resp = request(&gw, Method::GET, "/openapi.json", &[], None).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(resp.header("content-type"), Some("application/json"));
    assert_eq!(resp.header("cache-control"), Some("no-store"));
    let id = resp.header("x-request-id").expect("x-request-id");
    assert!(id.starts_with("gw_"), "{id}");
    assert_eq!(resp.header("x-typesafe-request-id"), Some(id));
    let doc = resp.json();
    assert_eq!(doc["openapi"], "3.1.0");
    assert_eq!(doc["info"]["version"], env!("CARGO_PKG_VERSION"));

    let resp = request(&gw, Method::POST, "/openapi.json", &[], None).await;
    assert_official_error(&resp, StatusCode::NOT_FOUND, Some("Not Found"));

    // Every documented operation reaches a handler (no 404).
    let auth = format!("Bearer {GATEWAY_KEY}");
    for (path, item) in doc["paths"].as_object().expect("paths") {
        for (name, method) in [("get", Method::GET), ("post", Method::POST)] {
            if item.get(name).is_none() {
                continue;
            }
            let resp = request(
                &gw,
                method.clone(),
                path,
                &[(header::AUTHORIZATION.as_str(), &auth)],
                None,
            )
            .await;
            assert_ne!(resp.status, StatusCode::NOT_FOUND, "{method} {path}");
        }
    }
    assert!(mock.requests().is_empty());
}

#[tokio::test]
async fn large_state_is_accepted_and_bodies_over_the_limit_are_413() {
    let mock = spawn_mock(MockOptions::default());
    let gw = spawn_mixed(&mock, &[]);

    // 100 KiB of state: above the old 65,536-byte limit, below MAX_BODY_BYTES.
    let mut request = example("jev-openai");
    request["state"]["ticket"] = json!("x".repeat(100 * 1024));
    let resp = post_systemone(&gw, &[], &request).await;
    assert_official_success(&resp, &request);

    // Default MAX_BODY_BYTES is 524,288.
    request["state"]["ticket"] = json!("x".repeat(600 * 1024));
    let resp = post_systemone(&gw, &[], &request).await;
    assert_official_error(&resp, StatusCode::PAYLOAD_TOO_LARGE, None);
    assert_eq!(upstream_calls(&mock, "/v1/responses"), 1);
}

/// Writes `head` on a raw connection, then stalls; returns everything the
/// gateway sends before closing (bounded so a missing timeout fails fast).
async fn send_and_stall(gw: &Gateway, head: &str) -> Vec<u8> {
    let mut stream = TcpStream::connect(gw.addr).await.expect("connect");
    stream.write_all(head.as_bytes()).await.expect("write");
    let mut out = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut out))
        .await
        .expect("gateway did not time out the stalled request")
        .expect("read");
    out
}

/// Parses a raw HTTP/1.1 response with a `Content-Length` body.
fn parse_raw(raw: &[u8]) -> Resp {
    let text = String::from_utf8_lossy(raw);
    let (head, body) = text.split_once("\r\n\r\n").expect("response head");
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .expect("status");
    let mut headers = hyper::HeaderMap::new();
    for line in lines {
        let (name, value) = line.split_once(':').expect("header line");
        headers.append(
            header::HeaderName::from_bytes(name.trim().as_bytes()).expect("header name"),
            value.trim().parse().expect("header value"),
        );
    }
    Resp {
        status: StatusCode::from_bytes(status.as_bytes()).expect("status code"),
        headers,
        body: body.to_owned().into(),
    }
}

#[tokio::test]
async fn slow_clients_get_408_or_are_disconnected() {
    let mock = spawn_mock(MockOptions::default());
    let gw = spawn_mixed(&mock, &[("REQUEST_READ_TIMEOUT_MS", "200")]);

    // Complete headers, then a stalled body: 408 in the official error shape.
    let head = format!(
        "POST /v1/systemone HTTP/1.1\r\nHost: gw\r\nAuthorization: Bearer {GATEWAY_KEY}\r\n\
         Content-Type: application/json\r\nContent-Length: 100\r\n\r\n{{\"model\""
    );
    let resp = parse_raw(&send_and_stall(&gw, &head).await);
    let detail = assert_official_error(&resp, StatusCode::REQUEST_TIMEOUT, None);
    assert_eq!(detail["error_type"], "request_timeout");
    assert_eq!(resp.json()["error"]["code"], "request_timeout");

    // Incomplete headers: Hyper's header read timeout closes the connection.
    let raw = send_and_stall(&gw, "POST /v1/systemone HTTP/1.1\r\nHost: gw\r\n").await;
    assert!(
        raw.is_empty() || raw.starts_with(b"HTTP/1.1 408"),
        "unexpected response: {}",
        String::from_utf8_lossy(&raw)
    );
    assert_eq!(upstream_calls(&mock, "/v1/responses"), 0);
}

#[tokio::test]
async fn idempotent_replay_returns_the_cached_answer_for_both_types() {
    let mock = spawn_mock(MockOptions::default());
    let gw = spawn_mixed(&mock, &[]);

    for (alias, mode, path) in [
        ("jev-openai", "responses-adapted", "/v1/responses"),
        ("jev-latest", "messages-adapted", "/v1/messages"),
    ] {
        let request = example(alias);
        let key = [("idempotency-key", alias)];
        let first = post_systemone(&gw, &key, &request).await;
        assert_official_success(&first, &request);
        assert!(first.header("x-idempotency-replayed").is_none());

        let replay = post_systemone(&gw, &key, &request).await;
        assert_official_success(&replay, &request);
        assert_eq!(replay.header("x-idempotency-replayed"), Some("true"));
        assert_eq!(replay.body, first.body, "{alias}: same body");
        assert_eq!(replay.header("x-system-one-response-mode"), Some(mode));
        assert_eq!(upstream_calls(&mock, path), 1, "{alias}: one upstream call");
    }
}

/// Keys of the flat JSON object that follows the first `"field":{` in `raw`,
/// in wire order. Keys and string values must not contain `,`, `:` or `}`.
fn wire_keys<'a>(raw: &'a str, field: &str) -> Vec<&'a str> {
    let open = format!("\"{field}\":{{");
    let start = raw
        .find(&open)
        .unwrap_or_else(|| panic!("{field} in {raw}"))
        + open.len();
    let end = start + raw[start..].find('}').expect("object end");
    raw[start..end]
        .split(',')
        .map(|entry| entry.split(':').next().unwrap().trim_matches('"'))
        .collect()
}

/// `examples/request.json` as raw bytes for `model`, keeping its key order
/// (questions team, urgency, is_blocking; team options incident_response,
/// billing, general_support). A `Value` round trip would sort the keys.
fn example_bytes(model: &str) -> Vec<u8> {
    include_str!("../../../examples/request.json")
        .replacen(
            "\"model\": \"jev-latest\"",
            &format!("\"model\": \"{model}\""),
            1,
        )
        .into_bytes()
}

/// Asserts each of `fields` appears in `raw`, in this order.
fn assert_in_order(raw: &str, fields: &[&str], ctx: &str) {
    let positions: Vec<usize> = fields
        .iter()
        .map(|f| raw.find(f).unwrap_or_else(|| panic!("{ctx}: {f} in {raw}")))
        .collect();
    assert!(positions.is_sorted(), "{ctx}: {fields:?} in {raw}");
}

#[tokio::test]
async fn success_bodies_follow_request_order_and_official_field_order() {
    let mock = spawn_mock(MockOptions::default());
    let gw = spawn_mixed(&mock, &[]);
    let auth = format!("Bearer {GATEWAY_KEY}");
    let headers = [
        (header::AUTHORIZATION.as_str(), auth.as_str()),
        (header::CONTENT_TYPE.as_str(), "application/json"),
    ];

    for alias in ["jev-openai", "jev-latest"] {
        let bytes = example_bytes(alias);
        let resp = request(&gw, Method::POST, "/v1/systemone", &headers, Some(bytes)).await;
        assert_official_success(&resp, &example(alias));
        let raw = std::str::from_utf8(&resp.body).expect("utf-8 body");

        // Questions in request order.
        assert_in_order(
            raw,
            &["\"team\":{", "\"urgency\":{", "\"is_blocking\":{"],
            alias,
        );
        // Choice: official field order, options in request order.
        let team = &raw[raw.find("\"team\":{").unwrap()..raw.find("\"urgency\":{").unwrap()];
        assert_in_order(
            team,
            &[
                "\"type\"",
                "\"choice\"",
                "\"confidence\"",
                "\"probabilities\"",
            ],
            alias,
        );
        assert_eq!(
            wire_keys(team, "probabilities"),
            ["incident_response", "billing", "general_support"],
            "{alias}"
        );
        // Score: official field order, levels in numeric order.
        let urgency =
            &raw[raw.find("\"urgency\":{").unwrap()..raw.find("\"is_blocking\":{").unwrap()];
        assert_in_order(
            urgency,
            &[
                "\"type\"",
                "\"score\"",
                "\"confidence\"",
                "\"legend\"",
                "\"probabilities\"",
            ],
            alias,
        );
        assert_eq!(wire_keys(urgency, "legend"), ["0", "1", "2"], "{alias}");
        assert_eq!(
            wire_keys(urgency, "probabilities"),
            ["0", "1", "2"],
            "{alias}"
        );
        // Noul: {type, noul}.
        assert!(
            raw.contains("\"is_blocking\":{\"type\":\"noul\",\"noul\":"),
            "{raw}"
        );
    }

    // Duplicate keys are still rejected before the order-keeping parse.
    let duplicate = example_bytes("jev-latest");
    let duplicate = String::from_utf8(duplicate).unwrap().replacen(
        "\"billing\":",
        "\"billing\": \"x\", \"billing\":",
        1,
    );
    let resp = request(
        &gw,
        Method::POST,
        "/v1/systemone",
        &headers,
        Some(duplicate.into_bytes()),
    )
    .await;
    let detail = assert_official_error(&resp, StatusCode::BAD_REQUEST, None);
    assert_eq!(detail["error_type"], "invalid_json");
}

#[tokio::test]
async fn idempotency_compares_request_bytes() {
    let mock = spawn_mock(MockOptions::default());
    let gw = spawn_mixed(&mock, &[]);
    let auth = format!("Bearer {GATEWAY_KEY}");
    let headers = [
        (header::AUTHORIZATION.as_str(), auth.as_str()),
        (header::CONTENT_TYPE.as_str(), "application/json"),
        ("idempotency-key", "ordered"),
    ];
    let bytes = example_bytes("jev-latest");
    let send = |body: Vec<u8>| request(&gw, Method::POST, "/v1/systemone", &headers, Some(body));

    let first = send(bytes.clone()).await;
    assert_official_success(&first, &example("jev-latest"));
    let replay = send(bytes).await;
    assert_eq!(replay.header("x-idempotency-replayed"), Some("true"));
    assert_eq!(replay.body, first.body);
    // The same request with its keys reordered is a different body.
    let sorted = serde_json::to_vec(&example("jev-latest")).unwrap();
    let resp = send(sorted).await;
    let detail = assert_official_error(&resp, StatusCode::CONFLICT, None);
    assert_eq!(detail["error_type"], "idempotency_conflict");
    assert_eq!(upstream_calls(&mock, "/v1/messages"), 1);
}

#[tokio::test]
async fn idempotency_conflicts_are_409() {
    let mock = spawn_mock(MockOptions::default());
    let providers = json!({
        RESPONSES_PROVIDER: mock_provider(&mock, "responses-api", "ok"),
        "upstream-rejecting": mock_provider(&mock, "responses-api", "http_400"),
    })
    .to_string();
    let routes = json!({
        "jev-latest": {"provider": RESPONSES_PROVIDER, "model": RESPONSES_MODEL},
        "jev-rejecting": {"provider": "upstream-rejecting", "model": RESPONSES_MODEL}
    })
    .to_string();
    let gw = spawn_gateway(&[
        ("PROVIDERS_JSON", &providers),
        ("MODEL_ROUTES_JSON", &routes),
    ]);

    // Same key, different body.
    let request = example("jev-latest");
    let key = [("idempotency-key", "conflict")];
    assert_official_success(&post_systemone(&gw, &key, &request).await, &request);
    let mut changed = request.clone();
    changed["state"]["region"] = json!("MY");
    let resp = post_systemone(&gw, &key, &changed).await;
    let detail = assert_official_error(&resp, StatusCode::CONFLICT, None);
    assert_eq!(detail["error_type"], "idempotency_conflict");

    // The original attempt failed upstream; reusing its key is a conflict.
    let failing = example("jev-rejecting");
    let key = [("idempotency-key", "failed-original")];
    let resp = post_systemone(&gw, &key, &failing).await;
    let detail = assert_official_error(&resp, StatusCode::BAD_GATEWAY, None);
    assert_eq!(detail["error_type"], "provider_rejected_request");
    let resp = post_systemone(&gw, &key, &failing).await;
    let detail = assert_official_error(&resp, StatusCode::CONFLICT, None);
    assert_eq!(detail["error_type"], "idempotency_failed");
    assert_eq!(upstream_calls(&mock, "/v1/responses"), 2);
}

#[tokio::test]
async fn admission_is_503_provider_busy_when_the_provider_is_full() {
    let mock = spawn_mock(MockOptions::default());
    let mut provider = mock_provider(&mock, "responses-api", "slow:500");
    provider["max_in_flight"] = json!(1);
    let providers = json!({ RESPONSES_PROVIDER: provider }).to_string();
    let gw = spawn_gateway(&[("PROVIDERS_JSON", &providers)]);
    let request = example("jev-latest");

    let (held, busy) = tokio::join!(post_systemone(&gw, &[], &request), async {
        // Let the first request take the only permit.
        tokio::time::sleep(Duration::from_millis(150)).await;
        post_systemone(&gw, &[], &request).await
    });
    let detail = assert_official_error(&busy, StatusCode::SERVICE_UNAVAILABLE, None);
    assert_eq!(detail["error_type"], "provider_busy");
    assert_official_success(&held, &request);
    assert_eq!(upstream_calls(&mock, "/v1/responses"), 1);
}

#[tokio::test]
async fn single_level_score_is_answered_by_both_types() {
    let mock = spawn_mock(MockOptions::default());
    let gw = spawn_mixed(&mock, &[]);

    for alias in ["jev-openai", "jev-latest"] {
        let mut request = example(alias);
        request["questions"]["urgency"]["criteria"] = json!(["Only level"]);
        let resp = post_systemone(&gw, &[], &request).await;
        let body = assert_official_success(&resp, &request);
        assert_eq!(
            body["answers"]["urgency"],
            json!({"type": "score", "score": 0.0, "confidence": 1.0,
                   "legend": {"0": "Only level"}, "probabilities": {"0": 1.0}}),
            "{alias}"
        );

        request["questions"]["urgency"]["criteria"] = json!([]);
        let resp = post_systemone(&gw, &[], &request).await;
        let detail = assert_official_error(&resp, StatusCode::UNPROCESSABLE_ENTITY, None);
        assert_eq!(detail["error_type"], "invalid_request", "{alias}");
    }
    let captured = mock.requests();
    assert_eq!(captured.len(), 2, "one upstream call per type");
    for call in captured {
        assert_eq!(call["contract_errors"], json!([]), "{call}");
    }
}

/// Question and state rules (P7): each type has a 422, and the two lighter
/// forms (a noul pair without instructions, a choice with instructions and
/// empty descriptions) are answered.
#[tokio::test]
async fn question_and_state_rules_reject_empty_text_and_accept_described_questions() {
    let mock = spawn_mock(MockOptions::default());
    let gw = spawn_mixed(&mock, &[]);

    let mut answered = example("jev-openai");
    answered["questions"]["is_blocking"] = json!({
        "type": "noul",
        "criteria": {
            "Agree": "Normal business operation is blocked right now.",
            "Disagree": "Business operation continues despite the issue."
        }
    });
    answered["questions"]["team"]["criteria"] =
        json!({"incident_response": "", "billing": null, "general_support": ""});
    let resp = post_systemone(&gw, &[], &answered).await;
    let body = assert_official_success(&resp, &answered);
    assert_eq!(body["answers"]["is_blocking"]["type"], "noul");
    let upstream = mock.requests()[0]["body"].to_string();
    assert!(
        upstream.contains(r#"{\"true\":\"Normal business operation is blocked right now.\""#),
        "{upstream}"
    );
    assert!(
        upstream.contains(r#"{\"o0\":{\"billing\":\"\"}}"#),
        "{upstream}"
    );

    let ask = "Evaluate operational urgency.";
    for (question, value, message) in [
        (
            "is_blocking",
            json!({"type": "noul", "criteria": "Is it blocked?"}),
            "question 'is_blocking': string criteria need instructions",
        ),
        (
            "team",
            json!({"type": "choice", "criteria": {"a": "x", "b": "y"}}),
            "question 'team': choice needs instructions",
        ),
        (
            "urgency",
            json!({"type": "score", "instructions": "Urgency?", "criteria": ["low", "high"]}),
            "question 'urgency': score needs instructions",
        ),
        (
            "is_blocking",
            json!({"type": "noul", "instructions": ask, "criteria": {"yes": "a", "false": "b"}}),
            "question 'is_blocking': criteria keys must be one positive/negative pair",
        ),
    ] {
        let mut request = example("jev-openai");
        request["questions"][question] = value;
        let resp = post_systemone(&gw, &[], &request).await;
        let detail = assert_official_error(&resp, StatusCode::UNPROCESSABLE_ENTITY, None);
        assert_eq!(detail["error_type"], "invalid_request");
        let got = detail["message"].as_str().unwrap();
        assert!(got.starts_with(message), "{got}");
    }

    for state in [
        json!({"temp": 38.5, "fever": true}),
        json!([1, true]),
        json!(null),
    ] {
        let mut request = example("jev-openai");
        request["state"] = state;
        let resp = post_systemone(&gw, &[], &request).await;
        let detail = assert_official_error(&resp, StatusCode::UNPROCESSABLE_ENTITY, None);
        assert!(detail["message"].as_str().unwrap().starts_with("state "));
    }
    assert_eq!(
        mock.requests().len(),
        1,
        "rejected requests never go upstream"
    );
}
