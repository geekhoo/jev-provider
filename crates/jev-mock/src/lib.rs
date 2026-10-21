//! Deterministic mock upstream for the Responses API and the Messages API.
//!
//! Dev/test only: it enforces an independent reading of each API's request
//! rules and replays upstream failure modes on demand (`x-mock-scenario`).

mod answer;
mod capture;
mod contract;
mod reply;
mod scenario;
mod server;

use scenario::Scenario;
use serde_json::Value;
use server::State;
use std::{future::Future, net::SocketAddr, sync::Arc};
use tokio::{net::TcpListener, sync::oneshot};

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Default listen address for the `jev-mock` binary (`MOCK_BIND`).
pub const DEFAULT_BIND: &str = "127.0.0.1:8090";

#[derive(Debug, Clone)]
pub struct MockOptions {
    /// Scenario used when a request has no `x-mock-scenario` header.
    pub default_scenario: String,
    /// Capture ring size; 0 disables capture.
    pub capture: usize,
    /// Larger request bodies get 413 in the provider's error shape.
    pub max_body_bytes: usize,
    /// When set, the request key must equal it; otherwise any non-empty key.
    pub require_key: Option<String>,
}

impl Default for MockOptions {
    fn default() -> Self {
        Self {
            default_scenario: "ok".to_owned(),
            capture: 100,
            max_body_bytes: 4 * 1024 * 1024,
            require_key: None,
        }
    }
}

impl MockOptions {
    /// Reads `MOCK_SCENARIO`, `MOCK_CAPTURE`, `MOCK_MAX_BODY_BYTES` and
    /// `MOCK_REQUIRE_KEY`; unset or empty values keep the defaults.
    pub fn from_env() -> Result<Self, BoxError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, BoxError> {
        let get = |name: &str| lookup(name).filter(|v| !v.trim().is_empty());
        let number = |name: &str, default: usize| -> Result<usize, BoxError> {
            get(name).map_or(Ok(default), |v| {
                v.trim()
                    .parse()
                    .map_err(|_| format!("{name} must be a non-negative integer").into())
            })
        };
        let defaults = Self::default();
        let opts = Self {
            default_scenario: get("MOCK_SCENARIO").unwrap_or(defaults.default_scenario),
            capture: number("MOCK_CAPTURE", defaults.capture)?,
            max_body_bytes: number("MOCK_MAX_BODY_BYTES", defaults.max_body_bytes)?,
            require_key: get("MOCK_REQUIRE_KEY"),
        };
        Scenario::parse(&opts.default_scenario).map_err(|e| format!("MOCK_SCENARIO: {e}"))?;
        Ok(opts)
    }
}

/// Serves the mock on an already bound listener until `shutdown` resolves.
pub async fn serve(
    listener: TcpListener,
    opts: MockOptions,
    shutdown: impl Future<Output = ()> + Send,
) -> Result<(), BoxError> {
    server::run(listener, Arc::new(State::new(opts)), shutdown).await
}

/// Starts the mock on an ephemeral `127.0.0.1` port in the current Tokio
/// runtime. The server shuts down when the returned handle is dropped.
pub fn spawn(opts: MockOptions) -> MockServer {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock listener");
    listener
        .set_nonblocking(true)
        .expect("set mock listener non-blocking");
    let addr = listener.local_addr().expect("mock listener address");
    let listener = TcpListener::from_std(listener).expect("register mock listener");
    let state = Arc::new(State::new(opts));
    let (tx, rx) = oneshot::channel::<()>();
    tokio::spawn(server::run(listener, state.clone(), async {
        let _ = rx.await;
    }));
    MockServer {
        addr,
        state,
        _shutdown: tx,
    }
}

/// Handle to a spawned mock; dropping it stops the server.
pub struct MockServer {
    pub addr: SocketAddr,
    state: Arc<State>,
    // Dropping the sender resolves the server's shutdown future.
    _shutdown: oneshot::Sender<()>,
}

impl MockServer {
    /// Captured requests, newest last (same entries as `GET /_mock/requests`).
    pub fn requests(&self) -> Vec<Value> {
        self.state.capture.recent(None)
    }

    /// Clears captured requests and `flaky` counters.
    pub fn clear(&self) {
        self.state.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use http_body_util::{BodyExt, Full};
    use hyper::{Method, Request, StatusCode};
    use hyper_util::{
        client::legacy::{connect::HttpConnector, Client},
        rt::TokioExecutor,
    };
    use serde_json::json;
    use std::collections::HashMap;

    struct Reply {
        status: StatusCode,
        headers: hyper::HeaderMap,
        body: Value,
        len: usize,
    }

    async fn send(
        mock: &MockServer,
        method: Method,
        path: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> Reply {
        let client: Client<HttpConnector, Full<Bytes>> =
            Client::builder(TokioExecutor::new()).build_http();
        let mut req = Request::builder()
            .method(method)
            .uri(format!("http://{}{path}", mock.addr));
        for (name, value) in headers {
            req = req.header(*name, *value);
        }
        let res = client
            .request(req.body(Full::new(Bytes::from(body.to_owned()))).unwrap())
            .await
            .unwrap();
        let (parts, body) = res.into_parts();
        let bytes = body.collect().await.unwrap().to_bytes();
        Reply {
            status: parts.status,
            headers: parts.headers,
            body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            len: bytes.len(),
        }
    }

    const RESPONSES: &[(&str, &str)] = &[("authorization", "Bearer sk-test-secret")];
    const MESSAGES: &[(&str, &str)] = &[
        ("x-api-key", "sk-ant-test-secret"),
        ("anthropic-version", "2023-06-01"),
    ];
    const BODY: &str = r#"{"model":"m","input":"hi","instructions":"x"}"#;
    // Kept under the 64-byte limit used by `auth_scenarios_and_bounds`.
    const MESSAGES_BODY: &str = r#"{"model":"m","max_tokens":1,"messages":[{"role":"user"}]}"#;

    fn with<'a>(base: &[(&'a str, &'a str)], scenario: &'a str) -> Vec<(&'a str, &'a str)> {
        let mut headers = base.to_vec();
        headers.push(("x-mock-scenario", scenario));
        headers
    }

    #[test]
    fn options_from_lookup() {
        let env: HashMap<&str, &str> = [
            ("MOCK_SCENARIO", "flaky:2"),
            ("MOCK_CAPTURE", "5"),
            ("MOCK_MAX_BODY_BYTES", "1024"),
            ("MOCK_REQUIRE_KEY", "k"),
        ]
        .into();
        let opts = MockOptions::from_lookup(|k| env.get(k).map(|v| v.to_string())).unwrap();
        assert_eq!(opts.default_scenario, "flaky:2");
        assert_eq!((opts.capture, opts.max_body_bytes), (5, 1024));
        assert_eq!(opts.require_key.as_deref(), Some("k"));

        let opts = MockOptions::from_lookup(|_| None).unwrap();
        assert_eq!(opts.default_scenario, "ok");
        assert_eq!((opts.capture, opts.max_body_bytes), (100, 4_194_304));
        assert_eq!(opts.require_key, None);

        assert!(
            MockOptions::from_lookup(|k| (k == "MOCK_SCENARIO").then(|| "http_600".into()))
                .is_err()
        );
        assert!(MockOptions::from_lookup(|k| (k == "MOCK_CAPTURE").then(|| "-1".into())).is_err());
    }

    #[tokio::test]
    async fn serves_routes_and_captures_redacted_requests() {
        let mock = spawn(MockOptions::default());
        let health = send(&mock, Method::GET, "/healthz", &[], "").await;
        assert_eq!(
            (health.status, health.body),
            (StatusCode::OK, json!({ "status": "ok" }))
        );

        let ok = send(&mock, Method::POST, "/v1/responses", RESPONSES, BODY).await;
        assert_eq!(ok.status, StatusCode::OK);
        assert_eq!(ok.body["object"], "response");
        assert!(ok.headers.contains_key("x-request-id"));

        let ok = send(&mock, Method::POST, "/v1/messages", MESSAGES, MESSAGES_BODY).await;
        assert_eq!(ok.status, StatusCode::OK);
        assert_eq!(ok.body["type"], "message");
        assert!(ok.headers.contains_key("request-id"));

        let missing = send(&mock, Method::POST, "/nope", RESPONSES, "x").await;
        assert_eq!(missing.status, StatusCode::NOT_FOUND);
        assert_eq!(missing.body["error"]["type"], "invalid_request_error");

        let listed = send(&mock, Method::GET, "/_mock/requests?limit=2", &[], "").await;
        let requests = listed.body["requests"].as_array().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["path"], "/v1/messages");
        assert_eq!(requests[0]["headers"]["x-api-key"], "[redacted]");
        assert_eq!(requests[0]["scenario"], "ok");
        assert_eq!(requests[1]["path"], "/nope");
        assert_eq!(requests[1]["body"], "x");
        assert_eq!(mock.requests().len(), 3);
        assert_eq!(mock.requests()[0]["body"]["model"], "m");
        assert_eq!(mock.requests()[0]["contract_errors"], json!([]));
        assert!(!serde_json::to_string(&mock.requests())
            .unwrap()
            .contains("secret"));

        let cleared = send(&mock, Method::DELETE, "/_mock/requests", &[], "").await;
        assert_eq!(cleared.status, StatusCode::NO_CONTENT);
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn auth_scenarios_and_bounds() {
        let mock = spawn(MockOptions {
            max_body_bytes: 64,
            require_key: Some("sk-ant-test-secret".to_owned()),
            ..MockOptions::default()
        });
        let path = "/v1/messages";
        let no_key = send(&mock, Method::POST, path, &[], "{}").await;
        assert_eq!(no_key.status, StatusCode::UNAUTHORIZED);
        assert_eq!(no_key.body["error"]["type"], "authentication_error");
        let wrong = [("x-api-key", "other")];
        assert_eq!(
            send(&mock, Method::POST, path, &wrong, "{}").await.status,
            StatusCode::UNAUTHORIZED
        );

        let unknown = send(&mock, Method::POST, path, &with(MESSAGES, "http_600"), "{}").await;
        assert_eq!(unknown.status, StatusCode::BAD_REQUEST);
        assert!(unknown.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("http_600"));

        let too_big = "x".repeat(65);
        let large = send(&mock, Method::POST, path, MESSAGES, &too_big).await;
        assert_eq!(large.status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(large.body["error"]["type"], "request_too_large");

        let not_json = send(&mock, Method::POST, path, MESSAGES, "nope").await;
        assert_eq!(not_json.status, StatusCode::BAD_REQUEST);
        // `http_*` skips body checks.
        let http = send(
            &mock,
            Method::POST,
            path,
            &with(MESSAGES, "http_529"),
            "nope",
        )
        .await;
        assert_eq!(http.status.as_u16(), 529);
        assert_eq!(http.body["error"]["type"], "overloaded_error");

        let busy = send(&mock, Method::POST, path, &with(MESSAGES, "http_429"), "{}").await;
        assert_eq!(busy.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(busy.headers["retry-after"], "7");

        let size = send(
            &mock,
            Method::POST,
            path,
            &with(MESSAGES, "oversize:4096"),
            MESSAGES_BODY,
        )
        .await;
        assert_eq!((size.status, size.len), (StatusCode::OK, 4096));
        assert_eq!(mock.requests().len(), 8);
    }

    #[tokio::test]
    async fn flaky_counts_per_instance_until_cleared() {
        let mock = spawn(MockOptions {
            default_scenario: "flaky:2".to_owned(),
            ..MockOptions::default()
        });
        let path = "/v1/responses";
        let statuses = |mock| async move {
            let mut out = Vec::new();
            for _ in 0..3 {
                out.push(
                    send(mock, Method::POST, path, RESPONSES, BODY)
                        .await
                        .status
                        .as_u16(),
                );
            }
            out
        };
        assert_eq!(statuses(&mock).await, [503, 503, 200]);
        // A different exact value has its own counter.
        let other = send(&mock, Method::POST, path, &with(RESPONSES, "flaky:1"), BODY).await;
        assert_eq!(other.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(other.body["error"]["type"], "server_error");
        mock.clear();
        assert_eq!(statuses(&mock).await, [503, 503, 200]);
    }

    #[tokio::test]
    async fn slow_delays_then_succeeds() {
        let mock = spawn(MockOptions::default());
        let start = std::time::Instant::now();
        let reply = send(
            &mock,
            Method::POST,
            "/v1/responses",
            &with(RESPONSES, "slow:150"),
            BODY,
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert!(start.elapsed() >= std::time::Duration::from_millis(150));
    }

    #[tokio::test]
    async fn responses_reasoning_fields_are_checked() {
        let mock = spawn(MockOptions::default());
        let path = "/v1/responses";
        let ok = r#"{"model":"m","input":"hi","instructions":"x","reasoning":{"effort":"minimal"},"max_output_tokens":64}"#;
        assert_eq!(
            send(&mock, Method::POST, path, RESPONSES, ok).await.status,
            StatusCode::OK
        );
        for bad in [
            r#"{"model":"m","input":"hi","instructions":"x","reasoning":{"effort":"extreme"}}"#,
            r#"{"model":"m","input":"hi","instructions":"x","max_output_tokens":0}"#,
        ] {
            let reply = send(&mock, Method::POST, path, RESPONSES, bad).await;
            assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{bad}");
            assert_eq!(reply.body["error"]["type"], "invalid_request_error");
        }
    }
}
