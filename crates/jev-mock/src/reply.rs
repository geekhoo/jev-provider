use crate::{answer, scenario::Scenario};
use bytes::Bytes;
use http_body_util::Full;
use hyper::{header, Response, StatusCode};
use serde_json::{json, Value};

pub type MockResponse = Response<Full<Bytes>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Api {
    Responses,
    Messages,
}

impl Api {
    fn request_id_header(self) -> &'static str {
        match self {
            Self::Responses => "x-request-id",
            Self::Messages => "request-id",
        }
    }
}

/// A 200 reply for every non-`http_*` scenario (`slow` and `flaky` reach
/// here as `Ok`).
pub fn success(api: Api, scenario: &Scenario, request: &Value, seq: u64) -> MockResponse {
    let text = answer_text(api, request);
    let text = match scenario {
        Scenario::MaxTokens => first_half(&text).to_owned(),
        Scenario::BadJson => "not json {".to_owned(),
        Scenario::WrongShape(_) => json!({ "result": "wrong shape" }).to_string(),
        _ => text,
    };
    let mut body = match api {
        Api::Responses => responses_body(scenario, request, &text, seq),
        Api::Messages => messages_body(scenario, request, &text, seq),
    };
    if *scenario == Scenario::NoUsage {
        if let Some(body) = body.as_object_mut() {
            body.remove("usage");
        }
    }
    let mut bytes = serde_json::to_vec(&body).unwrap_or_default();
    if let Scenario::Oversize(size) = *scenario {
        // Trailing whitespace keeps the body valid JSON at the exact size.
        bytes.resize(bytes.len().max(size), b' ');
    }
    json_response(api, StatusCode::OK, bytes, seq)
}

/// An error reply in the provider's own error shape (§5.6).
pub fn error(api: Api, status: StatusCode, message: &str, seq: u64) -> MockResponse {
    let code = status.as_u16();
    let body = match api {
        Api::Responses => {
            let (kind, code) = match code {
                401 => ("invalid_request_error", json!("invalid_api_key")),
                429 => ("rate_limit_error", Value::Null),
                500..=599 => ("server_error", Value::Null),
                _ => ("invalid_request_error", Value::Null),
            };
            json!({ "error": { "message": message, "type": kind, "param": null, "code": code } })
        }
        Api::Messages => {
            let kind = match code {
                401 => "authentication_error",
                403 => "permission_error",
                404 => "not_found_error",
                413 => "request_too_large",
                429 => "rate_limit_error",
                529 => "overloaded_error",
                500..=599 => "api_error",
                _ => "invalid_request_error",
            };
            json!({
                "type": "error",
                "error": { "type": kind, "message": message },
                "request_id": request_id(seq),
            })
        }
    };
    let mut response = json_response(api, status, body.to_string().into_bytes(), seq);
    if code == 429 {
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, header::HeaderValue::from_static("7"));
    }
    response
}

fn json_response(api: Api, status: StatusCode, body: Vec<u8>, seq: u64) -> MockResponse {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .header(api.request_id_header(), request_id(seq))
        .body(Full::new(Bytes::from(body)))
        .expect("static response parts are valid")
}

fn request_id(seq: u64) -> String {
    format!("req_mock_{seq}")
}

fn responses_body(scenario: &Scenario, request: &Value, text: &str, seq: u64) -> Value {
    let content = if *scenario == Scenario::Refusal {
        json!([{ "type": "refusal", "refusal": "mock refusal" }])
    } else {
        json!([{ "type": "output_text", "text": text, "annotations": [] }])
    };
    let mut output = vec![json!({ "type": "reasoning", "id": "rs_mock", "summary": [] })];
    if *scenario != Scenario::NoText {
        output.push(json!({
            "type": "message",
            "id": "msg_mock",
            "role": "assistant",
            "status": "completed",
            "content": content,
        }));
    }
    let mut body = json!({
        "id": format!("resp_mock_{seq}"),
        "object": "response",
        "status": "completed",
        "model": request["model"],
        "output": output,
        "usage": {
            "input_tokens": 120,
            "output_tokens": 30,
            "total_tokens": 150,
            "input_tokens_details": { "cached_tokens": 0 },
            "output_tokens_details": { "reasoning_tokens": 0 },
        },
    });
    if *scenario == Scenario::MaxTokens {
        body["status"] = json!("incomplete");
        body["incomplete_details"] = json!({ "reason": "max_output_tokens" });
    }
    body
}

fn messages_body(scenario: &Scenario, request: &Value, text: &str, seq: u64) -> Value {
    let thinking = json!({ "type": "thinking", "thinking": "", "signature": "mock" });
    let thinking_enabled = request["thinking"]["type"] != "disabled";
    let content = match scenario {
        Scenario::Refusal => json!([]),
        Scenario::NoText => json!([thinking]),
        _ if thinking_enabled => json!([thinking, { "type": "text", "text": text }]),
        _ => json!([{ "type": "text", "text": text }]),
    };
    let mut body = json!({
        "id": format!("msg_mock_{seq}"),
        "type": "message",
        "role": "assistant",
        "model": request["model"],
        "content": content,
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {
            "input_tokens": 100,
            "cache_read_input_tokens": 50,
            "cache_creation_input_tokens": 25,
            "output_tokens": 30,
        },
    });
    match scenario {
        Scenario::Refusal => {
            body["stop_reason"] = json!("refusal");
            body["stop_details"] = json!({ "type": "refusal", "category": "cyber" });
        }
        Scenario::MaxTokens => body["stop_reason"] = json!("max_tokens"),
        _ => {}
    }
    body
}

/// The answer JSON text for the request's schema, or `{}` without one (§5.5).
fn answer_text(api: Api, request: &Value) -> String {
    find_schema(api, request)
        .map(|schema| answer::conforming(&schema))
        .unwrap_or_else(|| json!({}))
        .to_string()
}

fn find_schema(api: Api, request: &Value) -> Option<Value> {
    let (format, prompt) = match api {
        Api::Responses => (&request["text"]["format"], prompt_text(&request["input"])),
        Api::Messages => (
            &request["output_config"]["format"],
            last_user_text(&request["messages"]),
        ),
    };
    if format["type"] == "json_schema" && format["schema"].is_object() {
        return Some(format["schema"].clone());
    }
    answer::schema_from_text(&prompt?)
}

fn last_user_text(messages: &Value) -> Option<String> {
    let last = messages
        .as_array()?
        .iter()
        .rev()
        .find(|m| m["role"] == "user")?;
    prompt_text(&last["content"])
}

/// A string, or the concatenated `text` of an array of content blocks.
fn prompt_text(content: &Value) -> Option<String> {
    match content {
        Value::String(text) => Some(text.clone()),
        Value::Array(blocks) => Some(
            blocks
                .iter()
                .filter_map(|b| b["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        _ => None,
    }
}

fn first_half(text: &str) -> &str {
    let half = text.chars().count() / 2;
    let end = text.char_indices().nth(half).map_or(text.len(), |(i, _)| i);
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    async fn parts(response: MockResponse) -> (StatusCode, hyper::HeaderMap, Vec<u8>) {
        let (parts, body) = response.into_parts();
        let bytes = body.collect().await.unwrap().to_bytes().to_vec();
        (parts.status, parts.headers, bytes)
    }

    fn request() -> Value {
        json!({ "model": "m" })
    }

    #[tokio::test]
    async fn request_id_headers_per_api() {
        let (_, headers, _) = parts(success(Api::Responses, &Scenario::Ok, &request(), 7)).await;
        assert_eq!(headers["x-request-id"], "req_mock_7");
        let (_, headers, _) = parts(success(Api::Messages, &Scenario::Ok, &request(), 8)).await;
        assert_eq!(headers["request-id"], "req_mock_8");
        let (_, headers, body) = parts(error(
            Api::Messages,
            StatusCode::TOO_MANY_REQUESTS,
            "busy",
            9,
        ))
        .await;
        assert_eq!(headers["request-id"], "req_mock_9");
        assert_eq!(headers["retry-after"], "7");
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["request_id"], "req_mock_9");
        assert_eq!(body["error"]["type"], "rate_limit_error");
    }

    #[tokio::test]
    async fn success_bodies_match_reply_shapes() {
        let (status, _, body) = parts(success(Api::Responses, &Scenario::Ok, &request(), 1)).await;
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["id"], "resp_mock_1");
        assert_eq!(body["model"], "m");
        assert_eq!(body["output"][1]["content"][0]["text"], "{}");
        assert!(body.get("output_text").is_none());

        let req = json!({ "model": "m", "thinking": { "type": "disabled" } });
        let (_, _, body) = parts(success(Api::Messages, &Scenario::Ok, &req, 2)).await;
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["content"], json!([{ "type": "text", "text": "{}" }]));
        assert_eq!(body["usage"]["cache_read_input_tokens"], 50);
    }

    #[tokio::test]
    async fn no_usage_body_has_no_usage_key() {
        for api in [Api::Responses, Api::Messages] {
            let (_, _, body) = parts(success(api, &Scenario::NoUsage, &request(), 1)).await;
            let body: Value = serde_json::from_slice(&body).unwrap();
            assert!(body.get("usage").is_none(), "{api:?}: {body}");
            assert!(body.get("model").is_some());
        }
    }

    #[tokio::test]
    async fn oversize_body_has_requested_size_and_stays_json() {
        for (scenario, size) in [
            (Scenario::Oversize(5000), 5000),
            (
                Scenario::Oversize(crate::scenario::DEFAULT_OVERSIZE_BYTES),
                2 * 1024 * 1024,
            ),
        ] {
            let (_, _, body) = parts(success(Api::Messages, &scenario, &request(), 1)).await;
            assert_eq!(body.len(), size);
            assert!(serde_json::from_slice::<Value>(&body).is_ok());
        }
    }

    #[tokio::test]
    async fn scenario_variants_change_the_body() {
        let body = |api, scenario| async move {
            let (_, _, body) = parts(success(api, &scenario, &request(), 1)).await;
            serde_json::from_slice::<Value>(&body).unwrap()
        };
        let r = body(Api::Responses, Scenario::Refusal).await;
        assert_eq!(r["output"][1]["content"][0]["type"], "refusal");
        let r = body(Api::Responses, Scenario::MaxTokens).await;
        assert_eq!(r["status"], "incomplete");
        assert_eq!(r["incomplete_details"]["reason"], "max_output_tokens");
        assert_eq!(r["output"][1]["content"][0]["text"], "{");
        let r = body(Api::Responses, Scenario::NoText).await;
        assert_eq!(r["output"].as_array().unwrap().len(), 1);
        let r = body(Api::Responses, Scenario::BadJson).await;
        assert_eq!(r["output"][1]["content"][0]["text"], "not json {");
        let r = body(Api::Responses, Scenario::WrongShape(None)).await;
        assert_eq!(r["status"], "completed");
        let text = r["output"][1]["content"][0]["text"].as_str().unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(text).unwrap()["answers"],
            Value::Null
        );

        let m = body(Api::Messages, Scenario::Refusal).await;
        assert_eq!(m["content"], json!([]));
        assert_eq!(m["stop_reason"], "refusal");
        assert_eq!(m["stop_details"]["category"], "cyber");
        let m = body(Api::Messages, Scenario::MaxTokens).await;
        assert_eq!(m["stop_reason"], "max_tokens");
        let m = body(Api::Messages, Scenario::NoText).await;
        assert_eq!(m["content"].as_array().unwrap().len(), 1);
        assert_eq!(m["content"][0]["type"], "thinking");
        let m = body(Api::Messages, Scenario::WrongShape(Some(1))).await;
        assert_eq!(m["stop_reason"], "end_turn");
        let text = m["content"][1]["text"].as_str().unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(text).unwrap()["answers"],
            Value::Null
        );
    }

    #[tokio::test]
    async fn error_types_per_api() {
        let cases = [
            (Api::Responses, 401, "invalid_request_error"),
            (Api::Responses, 404, "invalid_request_error"),
            (Api::Responses, 429, "rate_limit_error"),
            (Api::Responses, 503, "server_error"),
            (Api::Messages, 400, "invalid_request_error"),
            (Api::Messages, 422, "invalid_request_error"),
            (Api::Messages, 401, "authentication_error"),
            (Api::Messages, 403, "permission_error"),
            (Api::Messages, 404, "not_found_error"),
            (Api::Messages, 413, "request_too_large"),
            (Api::Messages, 409, "invalid_request_error"),
            (Api::Messages, 529, "overloaded_error"),
            (Api::Messages, 500, "api_error"),
        ];
        for (api, code, kind) in cases {
            let status = StatusCode::from_u16(code).unwrap();
            let (got, _, body) = parts(error(api, status, "msg", 1)).await;
            let body: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(got, status);
            match api {
                Api::Responses => {
                    assert_eq!(body["error"]["type"], kind, "{code}");
                    assert_eq!(body["error"]["message"], "msg");
                    let code_field = if code == 401 {
                        json!("invalid_api_key")
                    } else {
                        Value::Null
                    };
                    assert_eq!(body["error"]["code"], code_field);
                }
                Api::Messages => {
                    assert_eq!(body["type"], "error");
                    assert_eq!(body["error"]["type"], kind, "{code}");
                }
            }
        }
    }

    #[test]
    fn first_half_respects_char_boundaries() {
        assert_eq!(first_half("{\"a\":1}"), "{\"a");
        assert_eq!(first_half("éé"), "é");
        assert_eq!(first_half(""), "");
    }
}
