//! Every §5.8 scenario row for both provider types against `jev-mock` (PLAN
//! §7.7 6C): caller-facing status and code, retries counted from the mock's
//! captured requests, and the upstream error body never echoed.

mod common;

use common::{
    assert_official_error, assert_official_success, mock_provider, post_systemone, spawn_gateway,
    spawn_mock, Resp,
};
use hyper::StatusCode;
use jev_mock::MockOptions;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

const API_TYPES: [&str; 2] = ["responses-api", "messages-api"];

/// Text the mock puts in every error body it builds for a scenario.
const MOCK_ERROR_TEXT: &str = "mock scenario";

fn example_request() -> Value {
    serde_json::from_str(include_str!("../../../examples/request.json")).expect("example JSON")
}

/// One scenario run: the gateway response, the mock's captured upstream
/// requests, and the caller-observed elapsed time.
struct Run {
    resp: Resp,
    calls: Vec<Value>,
    elapsed: Duration,
}

/// Posts the example through a gateway whose only provider is `api_type`
/// with `max_retries: 2`, `scenario`, and the `extra` provider fields.
async fn exercise(api_type: &str, scenario: &str, extra: Value) -> Run {
    let mock = spawn_mock(MockOptions::default());
    let mut provider = mock_provider(&mock, api_type, scenario);
    provider["max_retries"] = json!(2);
    for (k, v) in extra.as_object().expect("extra fields") {
        provider[k] = v.clone();
    }
    let providers = json!({ "upstream": provider }).to_string();
    let gw = spawn_gateway(&[("PROVIDERS_JSON", &providers)]);

    let started = Instant::now();
    let resp = post_systemone(&gw, &[], &example_request()).await;
    let elapsed = started.elapsed();
    let calls = mock.requests();
    for call in &calls {
        // http_* skips contract checks; every other scenario proves the
        // request was valid.
        assert_eq!(
            call["contract_errors"],
            json!([]),
            "{api_type} {scenario}: {call}"
        );
    }
    Run {
        resp,
        calls,
        elapsed,
    }
}

/// Asserts a success through `assert_official_success` and the call count.
fn assert_success(run: &Run, calls: usize, ctx: &str) -> Value {
    let body = assert_official_success(&run.resp, &example_request());
    assert_eq!(run.calls.len(), calls, "{ctx}: upstream calls");
    body
}

/// Asserts an official error with `error_type`, the call count, and that the
/// mock's error text is not echoed. Returns the message.
fn assert_error(
    run: &Run,
    status: StatusCode,
    error_type: &str,
    calls: usize,
    ctx: &str,
) -> String {
    let detail = assert_official_error(&run.resp, status, None);
    assert_eq!(detail["error_type"], error_type, "{ctx}: {detail}");
    let message = detail["message"].as_str().expect("message").to_owned();
    assert!(
        !message.contains(MOCK_ERROR_TEXT)
            && !String::from_utf8_lossy(&run.resp.body).contains(MOCK_ERROR_TEXT),
        "{ctx}: upstream error echoed: {detail}"
    );
    assert_eq!(run.calls.len(), calls, "{ctx}: upstream calls");
    message
}

#[tokio::test]
async fn ok_succeeds_in_one_call() {
    for api in API_TYPES {
        let run = exercise(api, "ok", json!({})).await;
        assert_success(&run, 1, api);
    }
}

#[tokio::test]
async fn no_usage_reports_null_token_counts() {
    for api in API_TYPES {
        let run = exercise(api, "no_usage", json!({})).await;
        let body = assert_success(&run, 1, api);
        assert_eq!(
            body["usage"],
            json!({"input_tokens": null, "output_tokens": null}),
            "{api}"
        );
    }
}

#[tokio::test]
async fn refusal_is_provider_refused_without_retry() {
    for api in API_TYPES {
        let run = exercise(api, "refusal", json!({})).await;
        let message = assert_error(
            &run,
            StatusCode::UNPROCESSABLE_ENTITY,
            "provider_refused",
            1,
            api,
        );
        if api == "messages-api" {
            assert!(message.contains("cyber"), "{api}: {message}");
        }
    }
}

#[tokio::test]
async fn max_tokens_is_output_truncated_without_retry() {
    for api in API_TYPES {
        let run = exercise(api, "max_tokens", json!({})).await;
        assert_error(
            &run,
            StatusCode::BAD_GATEWAY,
            "provider_output_truncated",
            1,
            api,
        );
    }
}

#[tokio::test]
async fn no_text_and_bad_json_are_retried_invalid_responses() {
    for api in API_TYPES {
        for scenario in ["no_text", "bad_json"] {
            let run = exercise(api, scenario, json!({})).await;
            let ctx = format!("{api} {scenario}");
            assert_error(
                &run,
                StatusCode::BAD_GATEWAY,
                "invalid_provider_response",
                3,
                &ctx,
            );
        }
    }
}

#[tokio::test]
async fn oversize_is_retried_invalid_response_naming_the_limit() {
    for api in API_TYPES {
        let run = exercise(api, "oversize", json!({})).await;
        let message = assert_error(
            &run,
            StatusCode::BAD_GATEWAY,
            "invalid_provider_response",
            3,
            api,
        );
        // Default MAX_UPSTREAM_BYTES; the mock pads to 2 MiB.
        assert!(
            message.contains("exceeded 1048576 bytes"),
            "{api}: {message}"
        );
    }
}

#[tokio::test]
async fn http_429_is_busy_with_retry_after() {
    for api in API_TYPES {
        let run = exercise(api, "http_429", json!({})).await;
        assert_error(&run, StatusCode::TOO_MANY_REQUESTS, "provider_busy", 3, api);
        assert_eq!(run.resp.header("retry-after"), Some("7"), "{api}");
    }
}

#[tokio::test]
async fn http_500_is_unavailable_and_retried_only_for_messages() {
    for (api, calls) in [("responses-api", 1), ("messages-api", 3)] {
        let run = exercise(api, "http_500", json!({})).await;
        assert_error(
            &run,
            StatusCode::INTERNAL_SERVER_ERROR,
            "provider_unavailable",
            calls,
            api,
        );
    }
}

#[tokio::test]
async fn http_529_is_busy_and_retried() {
    let status = StatusCode::from_u16(529).expect("529");
    for api in API_TYPES {
        let run = exercise(api, "http_529", json!({})).await;
        assert_error(&run, status, "provider_busy", 3, api);
    }
}

#[tokio::test]
async fn upstream_4xx_is_rejected_request_never_the_upstream_status() {
    for api in API_TYPES {
        for code in [400, 401, 403, 404, 422] {
            let run = exercise(api, &format!("http_{code}"), json!({})).await;
            let ctx = format!("{api} http_{code}");
            assert_ne!(run.resp.status.as_u16(), code, "{ctx}");
            assert_error(
                &run,
                StatusCode::BAD_GATEWAY,
                "provider_rejected_request",
                1,
                &ctx,
            );
        }
    }
}

#[tokio::test]
async fn slow_upstream_times_out_per_attempt_within_the_deadline() {
    for api in API_TYPES {
        let run = exercise(api, "slow:1000", json!({"timeout_ms": 200})).await;
        assert_error(
            &run,
            StatusCode::GATEWAY_TIMEOUT,
            "provider_timeout",
            3,
            api,
        );
        // 3 attempts × 200 ms + 20 + 40 ms backoff, plus slack; one full
        // 1000 ms upstream reply per attempt would take 3 s.
        assert!(
            run.elapsed >= Duration::from_millis(600),
            "{api}: {:?}",
            run.elapsed
        );
        assert!(
            run.elapsed < Duration::from_millis(3 * 200 + 60 + 1000),
            "{api}: {:?}",
            run.elapsed
        );
    }
}

#[tokio::test]
async fn flaky_succeeds_after_retries_and_exhausts_with_fewer() {
    for api in API_TYPES {
        let run = exercise(api, "flaky:2", json!({})).await;
        assert_success(&run, 3, api);

        let run = exercise(api, "flaky:2", json!({"max_retries": 1})).await;
        assert_error(
            &run,
            StatusCode::SERVICE_UNAVAILABLE,
            "provider_unavailable",
            2,
            api,
        );
    }
}

#[tokio::test]
async fn wrong_shape_is_a_retried_invalid_response() {
    for api in API_TYPES {
        let run = exercise(api, "wrong_shape", json!({"max_retries": 0})).await;
        let ctx = format!("{api} wrong_shape max_retries 0");
        assert_error(
            &run,
            StatusCode::BAD_GATEWAY,
            "invalid_provider_response",
            1,
            &ctx,
        );

        let run = exercise(api, "wrong_shape", json!({})).await;
        let ctx = format!("{api} wrong_shape max_retries 2");
        assert_error(
            &run,
            StatusCode::BAD_GATEWAY,
            "invalid_provider_response",
            3,
            &ctx,
        );

        let run = exercise(api, "wrong_shape:2", json!({})).await;
        assert_success(&run, 3, &format!("{api} wrong_shape:2"));
    }
}
