use bytes::Bytes;
use http_body_util::Full;
use hyper::{header, Response, StatusCode};
use serde_json::json;
use std::fmt::{Display, Formatter};

#[derive(Debug, Clone)]
pub struct GatewayError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    pub source: &'static str,
    pub retry_after: Option<String>,
}

impl GatewayError {
    pub fn local(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            source: "platform",
            retry_after: None,
        }
    }

    pub fn upstream(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            source: "upstream",
            retry_after: None,
        }
    }

    pub fn with_retry_after(mut self, retry_after: Option<String>) -> Self {
        self.retry_after = retry_after;
        self
    }

    pub fn into_response(self, request_id: &str) -> Response<Full<Bytes>> {
        let detail = match self.code {
            "not_found" => json!("Not Found"),
            "method_not_allowed" => json!("Method Not Allowed"),
            code => json!({ "error_type": error_type(code), "message": self.message }),
        };
        let body = serde_json::to_vec(&json!({
            "detail": detail,
            "error": { "code": self.code, "message": self.message, "request_id": request_id },
            "request_id": request_id
        }))
        .unwrap_or_else(|_| {
            b"{\"error\":{\"code\":\"internal_error\",\"message\":\"serialization failure\"}}"
                .to_vec()
        });

        let mut builder = Response::builder()
            .status(self.status)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::CACHE_CONTROL, "no-store")
            .header("x-request-id", request_id)
            .header("x-typesafe-request-id", request_id)
            .header("x-system-one-error-source", self.source)
            .header("x-system-one-error-code", self.code);
        if let Some(value) = self.retry_after {
            if let Ok(v) = header::HeaderValue::from_str(&value) {
                builder = builder.header(header::RETRY_AFTER, v);
            }
        }
        builder.body(Full::new(Bytes::from(body))).unwrap()
    }
}

/// TypeSafe `detail.error_type`: `authentication_error` for auth failures,
/// otherwise the gateway code (D15).
fn error_type(code: &'static str) -> &'static str {
    match code {
        "invalid_api_key" => "authentication_error",
        code => code,
    }
}

impl Display for GatewayError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for GatewayError {}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::body::Body;
    use serde_json::Value;
    use std::pin::Pin;
    use std::task::{Context, Poll, Waker};

    fn body_json(response: Response<Full<Bytes>>) -> Value {
        let mut body = response.into_body();
        let mut cx = Context::from_waker(Waker::noop());
        match Pin::new(&mut body).poll_frame(&mut cx) {
            Poll::Ready(Some(Ok(frame))) => {
                serde_json::from_slice(&frame.into_data().unwrap()).unwrap()
            }
            _ => panic!("expected a ready data frame"),
        }
    }

    #[test]
    fn auth_error_uses_authentication_error_detail() {
        let response = GatewayError::local(StatusCode::UNAUTHORIZED, "invalid_api_key", "bad key")
            .into_response("gw_0123456789abcdef");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = body_json(response);
        assert_eq!(body["detail"]["error_type"], "authentication_error");
        assert_eq!(body["detail"]["message"], "bad key");
        assert_eq!(body["error"]["code"], "invalid_api_key");
        assert_eq!(body["error"]["request_id"], "gw_0123456789abcdef");
        assert_eq!(body["request_id"], "gw_0123456789abcdef");
    }

    #[test]
    fn not_found_and_method_not_allowed_use_detail_strings() {
        let body = body_json(
            GatewayError::local(StatusCode::NOT_FOUND, "not_found", "route not found")
                .into_response("r1"),
        );
        assert_eq!(body["detail"], "Not Found");
        assert_eq!(body["error"]["code"], "not_found");

        let body = body_json(
            GatewayError::local(
                StatusCode::METHOD_NOT_ALLOWED,
                "method_not_allowed",
                "method not allowed",
            )
            .into_response("r1"),
        );
        assert_eq!(body["detail"], "Method Not Allowed");
        assert_eq!(body["error"]["code"], "method_not_allowed");
    }

    #[test]
    fn other_errors_use_gateway_code_as_error_type() {
        let body = body_json(
            GatewayError::local(StatusCode::UNPROCESSABLE_ENTITY, "invalid_request", "bad")
                .into_response("r1"),
        );
        assert_eq!(body["detail"]["error_type"], "invalid_request");
        assert_eq!(body["detail"]["message"], "bad");
        assert_eq!(body["error"]["code"], "invalid_request");
    }

    #[test]
    fn error_headers_carry_both_request_ids() {
        let response =
            GatewayError::upstream(StatusCode::TOO_MANY_REQUESTS, "rate_limited", "slow")
                .with_retry_after(Some("7".into()))
                .into_response("gw_0123456789abcdef");
        let headers = response.headers();
        assert_eq!(headers["x-request-id"], "gw_0123456789abcdef");
        assert_eq!(headers["x-typesafe-request-id"], headers["x-request-id"]);
        assert_eq!(headers["x-system-one-error-source"], "upstream");
        assert_eq!(headers["x-system-one-error-code"], "rate_limited");
        assert_eq!(headers[header::RETRY_AFTER], "7");
    }
}
