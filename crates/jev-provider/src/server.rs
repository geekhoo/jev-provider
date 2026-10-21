use crate::{
    config::{AppConfig, ModelInfo},
    upstream::UpstreamClient,
    BoxError,
};
use bytes::{Bytes, BytesMut};
use http_body_util::{BodyExt, Full};
use hyper::{
    body::Incoming, header, service::service_fn, HeaderMap, Method, Request, Response, StatusCode,
};
use hyper_util::{
    rt::{TokioExecutor, TokioIo, TokioTimer},
    server::graceful::GracefulShutdown,
};
use jev_core::{
    error::GatewayError,
    idempotency::{BeginResult, CachedResponse, IdempotencyCache},
    model::JevRequest,
    strict_json,
    validate::validate_request,
};
use std::{
    collections::BTreeMap,
    convert::Infallible,
    future::Future,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, LazyLock,
    },
};
use tokio::{net::TcpListener, time::Instant};

static REQUEST_SEQ: AtomicU64 = AtomicU64::new(1);

/// Paths `dispatch` serves; keep in sync with its match arms. Tests check that
/// `openapi.json` documents exactly these and that each one reaches a handler.
#[cfg(test)]
const ROUTES: [&str; 5] = [
    "/v1/systemone",
    "/v1/models",
    "/api/system-one/config",
    "/healthz",
    "/openapi.json",
];

/// The static OpenAPI document with `info.version` set to the crate version.
static OPENAPI: LazyLock<Bytes> = LazyLock::new(|| {
    Bytes::from(include_str!("openapi.json").replacen(
        "__CARGO_PKG_VERSION__",
        env!("CARGO_PKG_VERSION"),
        1,
    ))
});

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<AppConfig>,
    pub upstream: UpstreamClient,
    pub idempotency: Arc<IdempotencyCache>,
}

/// Accepts connections on `listener` until `shutdown` resolves, then drains
/// in-flight connections for at most `shutdown_grace`.
pub async fn run(
    listener: TcpListener,
    state: AppState,
    shutdown: impl Future<Output = ()> + Send,
) -> Result<(), BoxError> {
    let graceful = GracefulShutdown::new();
    let mut signal = std::pin::pin!(shutdown);
    let mut builder = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(state.config.request_read_timeout);

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let _ = stream.set_nodelay(true);
                let io = TokioIo::new(stream);
                let state = state.clone();
                let service = service_fn(move |req| handle(req, state.clone()));
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

    let grace = state.config.shutdown_grace;
    if tokio::time::timeout(grace, graceful.shutdown())
        .await
        .is_err()
    {
        eprintln!("graceful shutdown deadline reached");
    }
    Ok(())
}

async fn handle(
    req: Request<Incoming>,
    state: AppState,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let request_id = format!("gw_{:016x}", REQUEST_SEQ.fetch_add(1, Ordering::Relaxed));
    let response = match dispatch(req, &state, &request_id).await {
        Ok(x) => x,
        Err(e) => {
            let method_not_allowed = e.status == StatusCode::METHOD_NOT_ALLOWED;
            let mut response = e.into_response(&request_id);
            if method_not_allowed {
                // `/v1/systemone` is the only route that answers 405.
                response
                    .headers_mut()
                    .insert(header::ALLOW, header::HeaderValue::from_static("POST"));
            }
            response
        }
    };
    Ok(response)
}

async fn dispatch(
    mut req: Request<Incoming>,
    state: &AppState,
    request_id: &str,
) -> Result<Response<Full<Bytes>>, GatewayError> {
    // One deadline bounds every body read and drain for this request.
    let read_deadline = Instant::now() + state.config.request_read_timeout;
    match (req.method(), req.uri().path()) {
        (&Method::GET, "/healthz") => {
            return Ok(json_response(
                StatusCode::OK,
                request_id,
                br#"{"status":"ok"}"#.to_vec(),
            ))
        }
        (&Method::GET, "/openapi.json") => {
            return Ok(json_response(StatusCode::OK, request_id, OPENAPI.clone()))
        }
        (&Method::GET, "/api/system-one/config") => {
            let body = serde_json::to_vec(&serde_json::json!({
                "defaultModel": state.config.default_model.as_str(),
                "ready": true,
                "limits": {
                    "bytes": state.config.max_body_bytes,
                    "questions": state.config.max_questions,
                    "choiceOptions": state.config.max_choice_options,
                    "scoreLevels": state.config.max_score_levels,
                    "depth": state.config.max_json_depth
                }
            }))
            .map_err(internal_serialize)?;
            return Ok(json_response(StatusCode::OK, request_id, body));
        }
        (&Method::GET, "/v1/models") => {
            authenticate(req.headers(), &state.config)?;
            let body = serde_json::to_vec(&models_body(&state.config.models))
                .map_err(internal_serialize)?;
            return Ok(json_response(StatusCode::OK, request_id, body));
        }
        (&Method::POST, "/v1/systemone") => {}
        (_, "/v1/systemone") => {
            return Err(GatewayError::local(
                StatusCode::METHOD_NOT_ALLOWED,
                "method_not_allowed",
                "use POST /v1/systemone",
            ))
        }
        _ => {
            return Err(GatewayError::local(
                StatusCode::NOT_FOUND,
                "not_found",
                "endpoint not found",
            ))
        }
    }

    let auth_namespace = authenticate(req.headers(), &state.config)?;
    let content_type = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|x| x.to_str().ok())
        .unwrap_or("");
    if !content_type
        .to_ascii_lowercase()
        .starts_with("application/json")
    {
        return Err(GatewayError::local(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            "Content-Type must be application/json",
        ));
    }
    if let Some(len) = req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|x| x.to_str().ok())
        .and_then(|x| x.parse::<usize>().ok())
    {
        if len > state.config.max_body_bytes {
            discard_body(req.body_mut(), state.config.max_body_bytes, read_deadline).await;
            return Err(GatewayError::local(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request_too_large",
                format!("request exceeds {} bytes", state.config.max_body_bytes),
            ));
        }
    }

    let idempotency_key = parse_idempotency_key(&req)?;
    let bytes =
        read_request_bounded(req.body_mut(), state.config.max_body_bytes, read_deadline).await?;
    strict_json::parse(&bytes).map_err(|_| {
        GatewayError::local(
            StatusCode::BAD_REQUEST,
            "invalid_json",
            "request body must be valid UTF-8 JSON without duplicate object keys",
        )
    })?;
    // Deserialize the original bytes, not the parsed `Value` (its maps are
    // sorted), so questions and Choice options keep request order (P4).
    let jev: JevRequest = serde_json::from_slice(&bytes).map_err(|_| {
        GatewayError::local(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_request",
            "request JSON does not match the Jev contract",
        )
    })?;
    validate_request(
        &jev,
        state.config.max_questions,
        state.config.max_choice_options,
        state.config.max_score_levels,
        state.config.max_json_depth,
        state.config.min_description_words,
    )?;

    let cache_namespace = idempotency_key
        .as_ref()
        .map(|k| format!("{}:{}", auth_namespace, k));
    if let Some(key) = cache_namespace.as_ref() {
        match state.idempotency.begin(key.clone(), bytes.clone()) {
            BeginResult::Started => {}
            BeginResult::Replay(cached) => {
                return Ok(success_response(
                    cached,
                    true,
                    state.config.expose_upstream_headers,
                ))
            }
            BeginResult::InProgress => {
                return Err(GatewayError::local(
                    StatusCode::CONFLICT,
                    "request_in_progress",
                    "the idempotent request is still in progress",
                )
                .with_retry_after(Some("1".to_owned())))
            }
            BeginResult::Failed => {
                return Err(GatewayError::local(
                    StatusCode::CONFLICT,
                    "idempotency_failed",
                    "the original idempotent operation failed; use a new key for a new attempt",
                ))
            }
            BeginResult::Conflict => {
                return Err(GatewayError::local(
                    StatusCode::CONFLICT,
                    "idempotency_conflict",
                    "the same Idempotency-Key was used with a different request body",
                ))
            }
        }
    }

    let result = state.upstream.evaluate(&jev, &state.config).await;

    match result {
        Ok(result) => {
            let body_vec = serde_json::to_vec(&result.response).map_err(internal_serialize)?;
            let response = CachedResponse {
                request_id: request_id.to_owned(),
                body: Bytes::from(body_vec),
                upstream_request_id: result.upstream_request_id,
                provider: result.provider,
                upstream_model: result.upstream_model,
                api_type: result.api_type,
            };
            if let Some(key) = cache_namespace.as_ref() {
                state.idempotency.success(key, response.clone());
            }
            Ok(success_response(
                response,
                false,
                state.config.expose_upstream_headers,
            ))
        }
        Err(e) => {
            if let Some(key) = cache_namespace.as_ref() {
                state.idempotency.failed(key);
            }
            Err(e)
        }
    }
}

/// Official TypeSafe auth messages (8B probe P1, D17).
const MISSING_KEY_MESSAGE: &str = "Must supply an API key! Check your request and try again.";
const INVALID_KEY_MESSAGE: &str =
    "Cannot authenticate with the server. Please check your API key and try again.";

/// No usable Bearer token (missing header, another scheme, or an empty token)
/// → 403; a well-formed Bearer token that is not configured → 401 (D17).
/// The scheme name matches case-insensitively (RFC 7235).
fn authenticate(headers: &HeaderMap, config: &AppConfig) -> Result<String, GatewayError> {
    if config.api_keys.is_empty() {
        return Ok("anonymous".to_owned());
    }
    let token = headers
        .get(header::AUTHORIZATION)
        .map(|raw| raw.as_bytes())
        .and_then(|raw| match raw.split_at_checked(7) {
            Some((scheme, token)) if scheme.eq_ignore_ascii_case(b"bearer ") => Some(token),
            _ => None,
        })
        .filter(|token| !token.trim_ascii().is_empty());
    let Some(token) = token else {
        return Err(GatewayError::local(
            StatusCode::FORBIDDEN,
            "invalid_api_key",
            MISSING_KEY_MESSAGE,
        ));
    };
    let Some(token) = std::str::from_utf8(token)
        .ok()
        .filter(|x| config.api_keys.contains(*x))
    else {
        return Err(GatewayError::local(
            StatusCode::UNAUTHORIZED,
            "invalid_api_key",
            INVALID_KEY_MESSAGE,
        ));
    };
    // Do not keep the raw token in the cache key. This is a non-cryptographic namespace only.
    Ok(format!("k{:016x}", fnv1a(token.as_bytes())))
}

/// `/v1/models` body: public aliases only, sorted by name (D14, D20).
fn models_body(models: &BTreeMap<String, ModelInfo>) -> serde_json::Value {
    let models: Vec<_> = models
        .iter()
        .map(|(name, info)| {
            serde_json::json!({
                "name": name,
                "description": info.description,
                "release_date": info.release_date
            })
        })
        .collect();
    serde_json::json!({ "models": models })
}

fn parse_idempotency_key(req: &Request<Incoming>) -> Result<Option<String>, GatewayError> {
    let Some(raw) = req.headers().get("idempotency-key") else {
        return Ok(None);
    };
    let s = raw.to_str().map_err(|_| {
        GatewayError::local(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_idempotency_key",
            "Idempotency-Key must be visible ASCII",
        )
    })?;
    let len = s.len();
    if len == 0 || len > 255 || !s.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
        return Err(GatewayError::local(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_idempotency_key",
            "Idempotency-Key must contain 1..=255 visible ASCII bytes",
        ));
    }
    Ok(Some(s.to_owned()))
}

/// Reads the body up to `limit` bytes; `408 request_timeout` once `deadline` passes.
async fn read_request_bounded(
    body: &mut Incoming,
    limit: usize,
    deadline: Instant,
) -> Result<Bytes, GatewayError> {
    let mut out = BytesMut::with_capacity(limit.min(16 * 1024));
    loop {
        let Ok(frame) = tokio::time::timeout_at(deadline, body.frame()).await else {
            return Err(GatewayError::local(
                StatusCode::REQUEST_TIMEOUT,
                "request_timeout",
                "timed out while reading request body",
            ));
        };
        let Some(frame) = frame else { break };
        let frame = frame.map_err(|_| {
            GatewayError::local(
                StatusCode::BAD_REQUEST,
                "invalid_json",
                "failed while reading request body",
            )
        })?;
        if let Ok(data) = frame.into_data() {
            if out.len().saturating_add(data.len()) > limit {
                discard_body(body, limit, deadline).await;
                return Err(GatewayError::local(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "request_too_large",
                    format!("request exceeds {limit} bytes"),
                ));
            }
            out.extend_from_slice(&data);
        }
    }
    Ok(out.freeze())
}

/// Drains up to `limit` more request bytes before a 413, so the response reaches
/// the client instead of a TCP reset from closing with unread input. Stops at
/// `deadline`; the caller still answers 413.
async fn discard_body(body: &mut Incoming, limit: usize, deadline: Instant) {
    let mut seen = 0usize;
    while seen <= limit {
        match tokio::time::timeout_at(deadline, body.frame()).await {
            Ok(Some(Ok(frame))) => {
                if let Ok(data) = frame.into_data() {
                    seen = seen.saturating_add(data.len());
                }
            }
            _ => return,
        }
    }
}

fn success_response(
    cached: CachedResponse,
    replayed: bool,
    expose_upstream: bool,
) -> Response<Full<Bytes>> {
    let request_id = cached.request_id.as_str();
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CACHE_CONTROL, "no-store")
        .header("x-request-id", request_id)
        .header("x-typesafe-request-id", request_id)
        .header(
            "x-system-one-response-mode",
            cached.api_type.response_mode(),
        )
        .body(Full::new(cached.body))
        .unwrap();
    if replayed {
        response.headers_mut().insert(
            "x-idempotency-replayed",
            header::HeaderValue::from_static("true"),
        );
    }
    // Upstream provider and model IDs stay private unless the operator opts in (D20).
    if !expose_upstream {
        return response;
    }
    if let Ok(v) = header::HeaderValue::from_str(&cached.provider) {
        response.headers_mut().insert("x-upstream-provider", v);
    }
    if let Ok(v) = header::HeaderValue::from_str(&cached.upstream_model) {
        response.headers_mut().insert("x-upstream-model", v);
    }
    if let Some(upstream_id) = cached.upstream_request_id.as_deref() {
        if let Ok(v) = header::HeaderValue::from_str(upstream_id) {
            response.headers_mut().insert("x-upstream-request-id", v);
        }
    }
    response
}

fn json_response(
    status: StatusCode,
    request_id: &str,
    body: impl Into<Bytes>,
) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CACHE_CONTROL, "no-store")
        .header("x-request-id", request_id)
        .header("x-typesafe-request-id", request_id)
        .body(Full::new(body.into()))
        .unwrap()
}

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in data {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn internal_serialize(_: serde_json::Error) -> GatewayError {
    GatewayError::local(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal_error",
        "response serialization failed",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keyed_config() -> AppConfig {
        AppConfig::from_lookup(&|name| match name {
            "ANTHROPIC_API_KEY" => Some("sk-a".to_owned()),
            "GATEWAY_API_KEYS" => Some("good-key".to_owned()),
            _ => None,
        })
        .unwrap()
    }

    fn auth_error(config: &AppConfig, authorization: Option<&[u8]>) -> Option<GatewayError> {
        let mut headers = HeaderMap::new();
        if let Some(value) = authorization {
            headers.insert(
                header::AUTHORIZATION,
                header::HeaderValue::from_bytes(value).unwrap(),
            );
        }
        authenticate(&headers, config).err().inspect(|e| {
            assert_eq!(e.code, "invalid_api_key");
        })
    }

    fn auth_status(config: &AppConfig, authorization: Option<&[u8]>) -> Option<StatusCode> {
        auth_error(config, authorization).map(|e| e.status)
    }

    #[test]
    fn no_usable_token_is_403_and_unknown_token_is_401() {
        let c = keyed_config();
        assert_eq!(auth_status(&c, None), Some(StatusCode::FORBIDDEN));
        for missing in [
            &b"Bearer "[..],
            b"Bearer",
            b"Bearer   ",
            b"Basic x",
            b"bearer ",
            b"good-key",
        ] {
            let e = auth_error(&c, Some(missing)).expect("rejected");
            assert_eq!(e.status, StatusCode::FORBIDDEN, "{missing:?}");
            assert_eq!(e.message, MISSING_KEY_MESSAGE);
        }
        for wrong in [
            &b"Bearer wrong"[..],
            b"Bearer good-key\xff",
            b"Bearer  good-key",
            b"BEARER wrong",
        ] {
            let e = auth_error(&c, Some(wrong)).expect("rejected");
            assert_eq!(e.status, StatusCode::UNAUTHORIZED, "{wrong:?}");
            assert_eq!(e.message, INVALID_KEY_MESSAGE);
        }
        for scheme in ["Bearer", "bearer", "BEARER", "bEaReR"] {
            let header = format!("{scheme} good-key");
            assert_eq!(auth_status(&c, Some(header.as_bytes())), None, "{scheme}");
        }
    }

    #[test]
    fn no_configured_keys_skips_auth() {
        let c = AppConfig::from_lookup(&|name| {
            (name == "ANTHROPIC_API_KEY").then(|| "sk-a".to_owned())
        })
        .unwrap();
        assert_eq!(auth_status(&c, None), None);
        assert_eq!(auth_status(&c, Some(b"Basic x")), None);
    }

    #[test]
    fn models_body_lists_public_catalog_sorted_by_name() {
        let info = |description: &str, release_date: &str| ModelInfo {
            description: description.to_owned(),
            release_date: release_date.to_owned(),
        };
        let models = BTreeMap::from([
            ("jev-z".to_owned(), info("", "2026-10-13")),
            ("jev-a".to_owned(), info("Fast", "2024-02-29")),
        ]);
        assert_eq!(
            models_body(&models),
            serde_json::json!({"models": [
                {"name": "jev-a", "description": "Fast", "release_date": "2024-02-29"},
                {"name": "jev-z", "description": "", "release_date": "2026-10-13"}
            ]})
        );
    }

    fn openapi() -> serde_json::Value {
        serde_json::from_slice(&OPENAPI).expect("openapi.json is valid JSON")
    }

    fn file_json(text: &str) -> serde_json::Value {
        serde_json::from_str(text).expect("valid JSON file")
    }

    #[test]
    fn openapi_document_has_version_and_documents_exactly_the_routes() {
        let doc = openapi();
        assert_eq!(doc["openapi"], "3.1.0");
        assert_eq!(doc["info"]["version"], env!("CARGO_PKG_VERSION"));
        let mut paths: Vec<&str> = doc["paths"]
            .as_object()
            .expect("paths object")
            .keys()
            .map(String::as_str)
            .collect();
        let mut routes = ROUTES.to_vec();
        paths.sort_unstable();
        routes.sort_unstable();
        assert_eq!(paths, routes);
    }

    #[test]
    fn openapi_user_instructions_equal_the_schema_file() {
        let schema = file_json(include_str!(
            "../../../schema/user-instructions.schema.json"
        ));
        assert_eq!(
            openapi()["components"]["schemas"]["UserInstructions"],
            schema
        );
    }

    /// The request example is `examples/request.json`, and the request and
    /// response examples carry every field their schemas mark as required.
    #[test]
    fn openapi_examples_have_every_required_field() {
        let doc = openapi();
        let op = &doc["paths"]["/v1/systemone"]["post"];
        let request =
            &op["requestBody"]["content"]["application/json"]["examples"]["routing"]["value"];
        assert_eq!(
            *request,
            file_json(include_str!("../../../examples/request.json"))
        );
        let response =
            &op["responses"]["200"]["content"]["application/json"]["examples"]["routing"]["value"];
        for (schema, value) in [("JevRequest", request), ("JevResponse", response)] {
            let schema = serde_json::json!({ "$ref": format!("#/components/schemas/{schema}") });
            let missing = missing_required(&doc, &schema, value, "$");
            assert!(missing.is_empty(), "{missing:?}");
        }

        let mut broken = request.clone();
        broken.as_object_mut().unwrap().remove("state");
        broken["questions"]["team"]
            .as_object_mut()
            .unwrap()
            .remove("criteria");
        let schema = serde_json::json!({ "$ref": "#/components/schemas/JevRequest" });
        let missing = missing_required(&doc, &schema, &broken, "$");
        assert!(
            missing.contains(&"$.state: missing".to_owned()),
            "{missing:?}"
        );
        assert!(
            missing.contains(&"$.questions.team.criteria: missing".to_owned()),
            "{missing:?}"
        );
    }

    /// Structural check only (no JSON Schema validator): `required` keys are
    /// present and `const` values match, following `$ref`, `properties`,
    /// `additionalProperties`, `items`, and `oneOf`/`anyOf` (one branch must pass).
    fn missing_required(
        doc: &serde_json::Value,
        schema: &serde_json::Value,
        value: &serde_json::Value,
        path: &str,
    ) -> Vec<String> {
        if let Some(r) = schema.get("$ref").and_then(|r| r.as_str()) {
            let target = doc.pointer(&r[1..]).expect("resolvable $ref");
            return missing_required(doc, target, value, path);
        }
        if let Some(c) = schema.get("const") {
            if c != value {
                return vec![format!("{path}: expected {c}")];
            }
        }
        for key in ["oneOf", "anyOf"] {
            if let Some(branches) = schema.get(key).and_then(|b| b.as_array()) {
                if value.is_null() && branches.iter().any(|b| b["type"] == "null") {
                    return Vec::new();
                }
                let results: Vec<_> = branches
                    .iter()
                    .map(|b| missing_required(doc, b, value, path))
                    .collect();
                if results.iter().any(Vec::is_empty) {
                    return Vec::new();
                }
                return results.concat();
            }
        }
        let mut out = Vec::new();
        if let Some(obj) = value.as_object() {
            let props = schema.get("properties").and_then(|p| p.as_object());
            for key in schema["required"].as_array().into_iter().flatten() {
                let key = key.as_str().expect("required key");
                if !obj.contains_key(key) {
                    out.push(format!("{path}.{key}: missing"));
                }
            }
            for (key, v) in obj {
                let sub = props
                    .and_then(|p| p.get(key))
                    .or_else(|| schema.get("additionalProperties"));
                if let Some(sub) = sub.filter(|s| s.is_object()) {
                    out.extend(missing_required(doc, sub, v, &format!("{path}.{key}")));
                }
            }
        }
        if let (Some(items), Some(xs)) = (schema.get("items"), value.as_array()) {
            for (i, v) in xs.iter().enumerate() {
                out.extend(missing_required(doc, items, v, &format!("{path}[{i}]")));
            }
        }
        out
    }
}
