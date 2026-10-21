use crate::{
    capture::{self, Capture},
    contract,
    reply::{self, Api, MockResponse},
    scenario::{FlakyCounters, Scenario},
    BoxError, MockOptions,
};
use bytes::{Bytes, BytesMut};
use http_body_util::{BodyExt, Full};
use hyper::{
    body::Incoming, header, service::service_fn, HeaderMap, Method, Request, Response, StatusCode,
};
use hyper_util::{
    rt::{TokioExecutor, TokioIo},
    server::graceful::GracefulShutdown,
};
use serde_json::{json, Value};
use std::{
    convert::Infallible,
    future::Future,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::net::TcpListener;

const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Shared per-instance state: options, capture ring, counters.
pub struct State {
    pub opts: MockOptions,
    pub capture: Capture,
    pub flaky: FlakyCounters,
    seq: AtomicU64,
}

impl State {
    pub fn new(opts: MockOptions) -> Self {
        Self {
            capture: Capture::new(opts.capture),
            flaky: FlakyCounters::default(),
            seq: AtomicU64::new(1),
            opts,
        }
    }

    pub fn clear(&self) {
        self.capture.clear();
        self.flaky.clear();
    }

    fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::Relaxed)
    }
}

/// Accepts connections until `shutdown` resolves, then drains briefly.
pub async fn run(
    listener: TcpListener,
    state: Arc<State>,
    shutdown: impl Future<Output = ()> + Send,
) -> Result<(), BoxError> {
    let graceful = GracefulShutdown::new();
    let mut signal = std::pin::pin!(shutdown);

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let _ = stream.set_nodelay(true);
                let io = TokioIo::new(stream);
                let state = state.clone();
                let service = service_fn(move |req| handle(req, state.clone()));
                let builder = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
                let conn = builder.serve_connection_with_upgrades(io, service).into_owned();
                let watched = graceful.watch(conn);
                tokio::spawn(async move {
                    if let Err(err) = watched.await { eprintln!("connection error: {err}"); }
                });
            }
            _ = &mut signal => {
                drop(listener);
                break;
            }
        }
    }

    let _ = tokio::time::timeout(SHUTDOWN_GRACE, graceful.shutdown()).await;
    Ok(())
}

async fn handle(req: Request<Incoming>, state: Arc<State>) -> Result<MockResponse, Infallible> {
    let response = match (req.method(), req.uri().path()) {
        (&Method::POST, "/v1/responses") => api_request(req, &state, Api::Responses).await,
        (&Method::POST, "/v1/messages") => api_request(req, &state, Api::Messages).await,
        (&Method::GET, "/_mock/requests") => {
            let limit = query_limit(req.uri().query());
            plain_json(
                StatusCode::OK,
                json!({ "requests": state.capture.recent(limit) }),
            )
        }
        (&Method::DELETE, "/_mock/requests") => {
            state.clear();
            Response::builder()
                .status(StatusCode::NO_CONTENT)
                .body(Full::new(Bytes::new()))
                .expect("static response parts are valid")
        }
        (&Method::GET, "/healthz") => plain_json(StatusCode::OK, json!({ "status": "ok" })),
        _ => not_found(req, &state).await,
    };
    Ok(response)
}

async fn api_request(req: Request<Incoming>, state: &State, api: Api) -> MockResponse {
    let seq = state.next_seq();
    let (parts, mut body) = req.into_parts();
    let scenario = parts
        .headers
        .get("x-mock-scenario")
        .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
        .unwrap_or_else(|| state.opts.default_scenario.clone());
    let record = |body: Value, errors: &[String]| {
        state.capture.push(capture::entry(
            seq,
            parts.method.as_str(),
            parts.uri.path(),
            Some(&scenario),
            &parts.headers,
            body,
            errors,
        ));
    };

    let bytes = match read_bounded(&mut body, state.opts.max_body_bytes).await {
        Ok(bytes) => bytes,
        Err((status, message)) => {
            record(Value::Null, std::slice::from_ref(&message));
            return reply::error(api, status, &message, seq);
        }
    };
    let (response, errors, delay) = decide(api, &parts.headers, &bytes, &scenario, state, seq);
    record(capture::body_json(&bytes), &errors);
    if let Some(delay) = delay {
        tokio::time::sleep(delay).await;
    }
    response
}

/// Auth → scenario → (unless `http_*`) JSON + contract checks → reply.
fn decide(
    api: Api,
    headers: &HeaderMap,
    bytes: &[u8],
    value: &str,
    state: &State,
    seq: u64,
) -> (MockResponse, Vec<String>, Option<Duration>) {
    let fail = |status: StatusCode, message: String| {
        (
            reply::error(api, status, &message, seq),
            vec![message],
            None,
        )
    };
    if let Err(message) = authenticate(api, headers, state.opts.require_key.as_deref()) {
        return fail(StatusCode::UNAUTHORIZED, message);
    }
    let scenario = match Scenario::parse(value) {
        Ok(scenario) => scenario,
        Err(message) => return fail(StatusCode::BAD_REQUEST, message),
    };
    if let Scenario::Http(status) = scenario {
        let status = StatusCode::from_u16(status).expect("scenario status is 400..=599");
        let message = format!("mock scenario {value}");
        return (reply::error(api, status, &message, seq), Vec::new(), None);
    }
    let request: Value = match serde_json::from_slice(bytes) {
        Ok(request) => request,
        Err(err) => {
            return fail(
                StatusCode::BAD_REQUEST,
                format!("request body is not valid JSON: {err}"),
            )
        }
    };
    let errors = match api {
        Api::Responses => contract::check_responses(headers, &request),
        Api::Messages => contract::check_messages(headers, &request),
    };
    if !errors.is_empty() {
        let response = reply::error(api, StatusCode::BAD_REQUEST, &errors.join("; "), seq);
        return (response, errors, None);
    }
    match scenario {
        Scenario::Flaky(n) if state.flaky.should_fail(value, n) => {
            let message = format!("mock scenario {value}");
            let response = reply::error(api, StatusCode::SERVICE_UNAVAILABLE, &message, seq);
            (response, Vec::new(), None)
        }
        Scenario::Flaky(_) => (
            reply::success(api, &Scenario::Ok, &request, seq),
            Vec::new(),
            None,
        ),
        Scenario::WrongShape(Some(n)) if !state.flaky.should_fail(value, n) => (
            reply::success(api, &Scenario::Ok, &request, seq),
            Vec::new(),
            None,
        ),
        Scenario::Slow(ms) => (
            reply::success(api, &Scenario::Ok, &request, seq),
            Vec::new(),
            Some(Duration::from_millis(ms)),
        ),
        other => (reply::success(api, &other, &request, seq), Vec::new(), None),
    }
}

/// Responses: `authorization: Bearer <key>`. Messages: `x-api-key` or a
/// bearer token. With `require_key`, the key must match it exactly.
fn authenticate(api: Api, headers: &HeaderMap, require_key: Option<&str>) -> Result<(), String> {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|k| !k.is_empty());
    let key = match api {
        Api::Responses => bearer.ok_or("missing or empty authorization: Bearer key")?,
        Api::Messages => headers
            .get("x-api-key")
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|k| !k.is_empty())
            .or(bearer)
            .ok_or("missing x-api-key or authorization: Bearer key")?,
    };
    match require_key {
        Some(required) if key != required => Err("invalid api key".to_owned()),
        _ => Ok(()),
    }
}

async fn read_bounded(body: &mut Incoming, limit: usize) -> Result<Bytes, (StatusCode, String)> {
    let mut out = BytesMut::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                "failed while reading request body".to_owned(),
            )
        })?;
        if let Ok(data) = frame.into_data() {
            if out.len().saturating_add(data.len()) > limit {
                return Err((
                    StatusCode::PAYLOAD_TOO_LARGE,
                    format!("request body exceeds {limit} bytes"),
                ));
            }
            out.extend_from_slice(&data);
        }
    }
    Ok(out.freeze())
}

/// Unknown routes: 404 in the Responses error shape; POSTs are captured too.
async fn not_found(req: Request<Incoming>, state: &State) -> MockResponse {
    let seq = state.next_seq();
    let message = format!("no route for {} {}", req.method(), req.uri().path());
    if req.method() == Method::POST {
        let (parts, mut body) = req.into_parts();
        let body = read_bounded(&mut body, state.opts.max_body_bytes)
            .await
            .map_or(Value::Null, |bytes| capture::body_json(&bytes));
        let scenario = parts
            .headers
            .get("x-mock-scenario")
            .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned());
        state.capture.push(capture::entry(
            seq,
            parts.method.as_str(),
            parts.uri.path(),
            scenario.as_deref(),
            &parts.headers,
            body,
            std::slice::from_ref(&message),
        ));
    }
    reply::error(Api::Responses, StatusCode::NOT_FOUND, &message, seq)
}

fn query_limit(query: Option<&str>) -> Option<usize> {
    query?
        .split('&')
        .find_map(|pair| pair.strip_prefix("limit="))
        .and_then(|n| n.parse().ok())
}

fn plain_json(status: StatusCode, body: Value) -> MockResponse {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(body.to_string())))
        .expect("static response parts are valid")
}
