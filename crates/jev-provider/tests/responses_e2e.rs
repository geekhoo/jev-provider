//! responses-api against `jev-mock`: every output mode, the exact §5.5
//! answers and §5.6 usage, the captured upstream request, and upstream keys.

mod common;

use common::{
    assert_official_error, assert_official_success, mock_provider, post_systemone, spawn_gateway,
    spawn_mock, PROVIDER_KEY, PROVIDER_KEY_ENV,
};
use hyper::StatusCode;
use jev_mock::{MockOptions, MockServer};
use serde_json::{json, Value};

const SCHEMA_MARKER: &str =
    "OUTPUT JSON SCHEMA (return exactly one JSON object matching this schema):\n";

/// `examples/request.json` addressed to `model`.
fn example(model: &str) -> Value {
    let mut request: Value =
        serde_json::from_str(include_str!("../../../examples/request.json")).expect("example");
    request["model"] = json!(model);
    request
}

/// `PROVIDERS_JSON` with one `ok` responses-api provider plus `extra` fields.
fn providers(mock: &MockServer, extra: Value) -> String {
    let mut provider = mock_provider(mock, "responses-api", "ok");
    for (k, v) in extra.as_object().expect("extra fields") {
        provider[k] = v.clone();
    }
    json!({ "upstream": provider }).to_string()
}

/// The §5.5 answers for `examples/request.json` and the §5.6 Responses usage.
fn assert_example_answers(body: &Value) {
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

    assert_eq!(
        body["usage"],
        json!({"input_tokens": 120, "output_tokens": 30})
    );
}

/// The only captured upstream request, checked for a clean contract and a
/// redacted Bearer key; returns its body.
fn single_capture(mock: &MockServer) -> Value {
    let requests = mock.requests();
    assert_eq!(requests.len(), 1, "upstream calls: {requests:?}");
    let captured = &requests[0];
    assert_eq!(captured["path"], "/v1/responses");
    assert_eq!(captured["contract_errors"], json!([]));
    // The mock redacts key values; it accepted the call, so it was a Bearer key.
    assert_eq!(captured["headers"]["authorization"], "[redacted]");
    assert!(captured["headers"].get("x-api-key").is_none());
    captured["body"].clone()
}

/// Every `number` in `schema` must carry the `[0, 1]` bounds; returns the count.
fn bounded_numbers(schema: &Value) -> usize {
    match schema {
        Value::Object(map) => {
            if map.get("type") == Some(&json!("number")) {
                assert_eq!(map.get("minimum"), Some(&json!(0)), "{schema}");
                assert_eq!(map.get("maximum"), Some(&json!(1)), "{schema}");
                return 1;
            }
            map.values().map(bounded_numbers).sum()
        }
        Value::Array(items) => items.iter().map(bounded_numbers).sum(),
        _ => 0,
    }
}

#[tokio::test]
async fn json_schema_mode_returns_exact_answers_with_strict_bounded_format() {
    let mock = spawn_mock(MockOptions::default());
    let gw = spawn_gateway(&[("PROVIDERS_JSON", &providers(&mock, json!({})))]);
    let request = example("jev-latest");

    let resp = post_systemone(&gw, &[], &request).await;
    let body = assert_official_success(&resp, &request);
    assert_eq!(
        resp.header("x-system-one-response-mode"),
        Some("responses-adapted")
    );
    assert_example_answers(&body);

    let sent = single_capture(&mock);
    assert_eq!(sent["store"], false, "store defaults to false");
    assert!(sent.get("reasoning").is_none(), "no effort configured");
    assert!(sent.get("max_output_tokens").is_none(), "no cap configured");
    let format = &sent["text"]["format"];
    assert_eq!(format["type"], "json_schema");
    assert_eq!(format["strict"], true);
    // 1 Noul probability + 3 Choice options + 3 Score levels.
    assert_eq!(bounded_numbers(&format["schema"]), 7);
    assert!(!sent["input"]
        .as_str()
        .expect("input string")
        .contains(SCHEMA_MARKER));
}

#[tokio::test]
async fn json_object_and_text_modes_return_exact_answers() {
    for mode in ["json_object", "text"] {
        let mock = spawn_mock(MockOptions::default());
        let providers = providers(&mock, json!({"output_mode": mode}));
        let gw = spawn_gateway(&[("PROVIDERS_JSON", &providers)]);
        let request = example("jev-latest");

        let resp = post_systemone(&gw, &[], &request).await;
        let body = assert_official_success(&resp, &request);
        assert_example_answers(&body);

        let sent = single_capture(&mock);
        let input = sent["input"].as_str().expect("input string");
        assert!(input.contains(SCHEMA_MARKER), "{mode}: schema in input");
        match mode {
            "json_object" => assert_eq!(sent["text"], json!({"format": {"type": "json_object"}})),
            _ => assert!(sent.get("text").is_none(), "text mode sends no format"),
        }
    }
}

#[tokio::test]
async fn store_follows_provider_config() {
    for (store, expected) in [
        (json!(true), Some(json!(true))),
        (json!(false), Some(json!(false))),
        (Value::Null, None),
    ] {
        let mock = spawn_mock(MockOptions::default());
        let providers = providers(&mock, json!({ "store": store }));
        let gw = spawn_gateway(&[("PROVIDERS_JSON", &providers)]);
        let request = example("jev-latest");

        let resp = post_systemone(&gw, &[], &request).await;
        assert_official_success(&resp, &request);
        let sent = single_capture(&mock);
        assert_eq!(sent.get("store").cloned(), expected, "store: {store}");
    }
}

#[tokio::test]
async fn route_effort_and_max_tokens_reach_upstream() {
    let mock = spawn_mock(MockOptions::default());
    let routes = json!({
        "jev-reasoning": {"provider": "upstream", "model": "gpt-6-astra", "effort": "high", "max_tokens": 4096}
    })
    .to_string();
    let gw = spawn_gateway(&[
        ("PROVIDERS_JSON", &providers(&mock, json!({}))),
        ("MODEL_ROUTES_JSON", &routes),
    ]);
    let request = example("jev-reasoning");

    let resp = post_systemone(&gw, &[], &request).await;
    let body = assert_official_success(&resp, &request);
    assert_example_answers(&body);

    let sent = single_capture(&mock);
    assert_eq!(sent["model"], "gpt-6-astra");
    assert_eq!(sent["reasoning"], json!({"effort": "high"}));
    assert_eq!(sent["max_output_tokens"], 4096);
}

#[tokio::test]
async fn upstream_key_match_succeeds_and_mismatch_is_rejected_request() {
    let opts = MockOptions {
        require_key: Some(PROVIDER_KEY.to_owned()),
        ..MockOptions::default()
    };
    let request = example("jev-latest");

    let mock = spawn_mock(opts.clone());
    let gw = spawn_gateway(&[("PROVIDERS_JSON", &providers(&mock, json!({})))]);
    let resp = post_systemone(&gw, &[], &request).await;
    assert_official_success(&resp, &request);

    let mock = spawn_mock(opts);
    let gw = spawn_gateway(&[
        ("PROVIDERS_JSON", &providers(&mock, json!({}))),
        (PROVIDER_KEY_ENV, "sk-wrong"),
    ]);
    let resp = post_systemone(&gw, &[], &request).await;
    let detail = assert_official_error(&resp, StatusCode::BAD_GATEWAY, None);
    assert_eq!(detail["error_type"], "provider_rejected_request");
    assert!(
        !detail["message"]
            .as_str()
            .unwrap_or_default()
            .contains("invalid api key"),
        "mock message echoed: {detail}"
    );
    assert_eq!(mock.requests().len(), 1, "a 401 is not retried");
}
