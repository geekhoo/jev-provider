//! messages-api end to end against `jev-mock` (PLAN §7.7 6B): every provider
//! and route variant succeeds with the §5.5 answers, and the captured request
//! body and headers follow §4.3.

mod common;

use common::{
    assert_official_error, assert_official_success, mock_provider, post_systemone, spawn_gateway,
    spawn_mock, Resp, PROVIDER_KEY,
};
use hyper::StatusCode;
use jev_core::prompt::SYSTEM_INSTRUCTIONS;
use jev_mock::MockOptions;
use serde_json::{json, Value};

const MARKER: &str = "OUTPUT JSON SCHEMA (return exactly one JSON object matching this schema):\n";
const BOUNDS: [&str; 7] = [
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "multipleOf",
    "minLength",
    "maxLength",
];

fn example_request() -> Value {
    serde_json::from_str(include_str!("../../../examples/request.json")).expect("example JSON")
}

/// Posts the example through a gateway whose one messages-api provider and
/// `jev-latest` route carry the `provider` and `route` extra fields. Returns
/// the response and the single request the mock captured.
async fn run(opts: MockOptions, provider: Value, route: Value) -> (Resp, Value) {
    let mock = spawn_mock(opts);
    let mut entry = mock_provider(&mock, "messages-api", "ok");
    merge(&mut entry, provider);
    let mut alias = json!({"provider": "anthropic", "model": "claude-haiku-5-5"});
    merge(&mut alias, route);
    let providers = json!({ "anthropic": entry }).to_string();
    let routes = json!({ "jev-latest": alias }).to_string();
    let gw = spawn_gateway(&[
        ("PROVIDERS_JSON", &providers),
        ("MODEL_ROUTES_JSON", &routes),
    ]);
    let resp = post_systemone(&gw, &[], &example_request()).await;
    let requests = mock.requests();
    assert_eq!(requests.len(), 1, "upstream calls: {requests:?}");
    (resp, requests.into_iter().next().unwrap())
}

fn merge(target: &mut Value, extra: Value) {
    let target = target.as_object_mut().expect("object");
    for (k, v) in extra.as_object().expect("extra object") {
        target.insert(k.clone(), v.clone());
    }
}

/// Official shape plus the exact §5.5 answers, §5.6 usage and route model.
fn assert_mock_answers(resp: &Resp) {
    assert_eq!(
        resp.header("x-system-one-response-mode"),
        Some("messages-adapted")
    );
    let body = assert_official_success(resp, &example_request());
    assert_eq!(body["model"], "jev-latest");
    assert_eq!(
        body["usage"],
        json!({"input_tokens": 175, "output_tokens": 30})
    );
    // Mock weights 1/6, 2/6, 3/6, rounded to 2 decimals (D23).
    let answers = &body["answers"];
    assert_eq!(answers["is_blocking"]["noul"], json!(0.75));
    let team = &answers["team"];
    assert_eq!(team["choice"], "incident_response");
    assert_eq!(
        team["probabilities"],
        json!({"billing": 0.17, "general_support": 0.33, "incident_response": 0.5})
    );
    assert_eq!(team["confidence"], json!(0.25));
    let urgency = &answers["urgency"];
    assert_eq!(
        urgency["probabilities"],
        json!({"0": 0.17, "1": 0.33, "2": 0.5})
    );
    assert_eq!(urgency["score"], json!(1.33));
    assert_eq!(urgency["confidence"], json!(0.0));
}

/// Shared §4.3 request checks: the mock accepted it, single user turn, route
/// defaults, adaptive/medium/16000 unless overridden, never-sent keys absent.
fn assert_common_request(captured: &Value) -> &Value {
    assert_eq!(captured["path"], "/v1/messages");
    assert_eq!(captured["contract_errors"], json!([]), "{captured}");
    assert_eq!(captured["headers"]["anthropic-version"], "2023-06-01");
    let body = &captured["body"];
    assert_eq!(body["model"], "claude-haiku-5-5");
    assert_eq!(body["max_tokens"], 16000);
    assert_eq!(body["output_config"]["effort"], "medium");
    let messages = body["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["role"], "user");
    assert!(messages[0]["content"].is_string());
    for key in [
        "temperature",
        "top_p",
        "top_k",
        "stop_sequences",
        "stream",
        "tools",
        "fallbacks",
    ] {
        assert!(body.get(key).is_none(), "{key} must not be sent");
    }
    assert!(body["thinking"].get("budget_tokens").is_none());
    body
}

fn assert_no_bounds(value: &Value, path: &str) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                assert!(!BOUNDS.contains(&k.as_str()), "{path}/{k}: bound sent");
                assert_no_bounds(v, &format!("{path}/{k}"));
            }
        }
        Value::Array(items) => {
            for (i, v) in items.iter().enumerate() {
                assert_no_bounds(v, &format!("{path}/{i}"));
            }
        }
        _ => {}
    }
}

fn has_bound(value: &Value) -> bool {
    match value {
        Value::Object(map) => map
            .iter()
            .any(|(k, v)| BOUNDS.contains(&k.as_str()) || has_bound(v)),
        Value::Array(items) => items.iter().any(has_bound),
        _ => false,
    }
}

#[tokio::test]
async fn default_provider_sends_cached_system_adaptive_schema_without_bounds() {
    let (resp, captured) = run(MockOptions::default(), json!({}), json!({})).await;
    assert_mock_answers(&resp);

    let headers = &captured["headers"];
    assert_eq!(headers["x-api-key"], "[redacted]");
    assert!(headers.get("authorization").is_none());
    assert!(headers.get("anthropic-beta").is_none());

    let body = assert_common_request(&captured);
    assert_eq!(
        body["system"],
        json!([{"type": "text", "text": SYSTEM_INSTRUCTIONS, "cache_control": {"type": "ephemeral"}}])
    );
    assert_eq!(body["thinking"], json!({"type": "adaptive"}));
    let format = &body["output_config"]["format"];
    assert_eq!(format["type"], "json_schema");
    assert!(format["schema"].is_object());
    assert!(format.get("name").is_none() && format.get("strict").is_none());
    assert_no_bounds(body, "");
    let content = body["messages"][0]["content"].as_str().unwrap();
    assert!(
        !content.contains(MARKER),
        "schema embedded outside prompt mode"
    );
}

#[tokio::test]
async fn bearer_auth_sends_authorization_instead_of_x_api_key() {
    let (resp, captured) = run(MockOptions::default(), json!({"auth": "bearer"}), json!({})).await;
    assert_mock_answers(&resp);
    let headers = &captured["headers"];
    assert_eq!(headers["authorization"], "[redacted]");
    assert!(headers.get("x-api-key").is_none());
    assert_no_bounds(assert_common_request(&captured), "");
}

#[tokio::test]
async fn betas_are_joined_into_anthropic_beta() {
    let provider = json!({"betas": ["beta-one-2026-01-01", "beta-two-2026-02-02"]});
    let (resp, captured) = run(MockOptions::default(), provider, json!({})).await;
    assert_mock_answers(&resp);
    assert_eq!(
        captured["headers"]["anthropic-beta"],
        "beta-one-2026-01-01,beta-two-2026-02-02"
    );
    assert_no_bounds(assert_common_request(&captured), "");
}

#[tokio::test]
async fn uncached_system_is_a_plain_string() {
    let (resp, captured) = run(
        MockOptions::default(),
        json!({"cache_system": false}),
        json!({}),
    )
    .await;
    assert_mock_answers(&resp);
    let body = assert_common_request(&captured);
    assert_eq!(body["system"], SYSTEM_INSTRUCTIONS);
    assert_no_bounds(body, "");
}

#[tokio::test]
async fn prompt_mode_embeds_schema_in_user_text_and_keeps_effort_and_thinking() {
    let (resp, captured) = run(
        MockOptions::default(),
        json!({"output_mode": "prompt"}),
        json!({}),
    )
    .await;
    assert_mock_answers(&resp);
    let body = assert_common_request(&captured);
    assert!(body["output_config"].get("format").is_none());
    assert_eq!(body["thinking"], json!({"type": "adaptive"}));
    let content = body["messages"][0]["content"].as_str().unwrap();
    let (_, schema) = content
        .rsplit_once(MARKER)
        .expect("schema marker in user text");
    let schema: Value = serde_json::from_str(schema).expect("embedded schema is JSON");
    // The text copy is the original schema; bounds are informative there.
    assert!(has_bound(&schema), "embedded schema keeps its bounds");
}

#[tokio::test]
async fn thinking_disabled_is_sent_and_succeeds_without_thinking_block() {
    let (resp, captured) = run(
        MockOptions::default(),
        json!({}),
        json!({"thinking": "disabled"}),
    )
    .await;
    assert_mock_answers(&resp);
    let body = assert_common_request(&captured);
    assert_eq!(body["thinking"], json!({"type": "disabled"}));
    assert_no_bounds(body, "");
}

#[tokio::test]
async fn required_key_match_succeeds() {
    let opts = MockOptions {
        require_key: Some(PROVIDER_KEY.to_owned()),
        ..MockOptions::default()
    };
    let (resp, _) = run(opts, json!({}), json!({})).await;
    assert_mock_answers(&resp);
}

#[tokio::test]
async fn required_key_mismatch_is_provider_rejected_request() {
    let opts = MockOptions {
        require_key: Some("sk-some-other-key".to_owned()),
        ..MockOptions::default()
    };
    let (resp, _) = run(opts, json!({}), json!({})).await;
    let detail = assert_official_error(&resp, StatusCode::BAD_GATEWAY, None);
    assert_eq!(detail["error_type"], "provider_rejected_request");
    let message = detail["message"].as_str().unwrap();
    assert!(
        message.contains("401"),
        "upstream status in message: {message}"
    );
    assert!(
        !message.contains("invalid api key"),
        "mock body echoed: {message}"
    );
}
