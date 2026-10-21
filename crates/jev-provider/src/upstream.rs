use crate::config::{AppConfig, ProviderRuntime};
use bytes::{Bytes, BytesMut};
use http_body_util::{BodyExt, Full};
use hyper::{header, Method, Request, StatusCode};
use hyper_rustls::HttpsConnector;
use hyper_util::{
    client::legacy::{connect::HttpConnector, Client},
    rt::{TokioExecutor, TokioTimer},
};
use jev_core::{
    error::GatewayError,
    map::map_output,
    model::{JevRequest, JevResponse, PreparedDecision, Usage},
    prompt::prepare_decision,
    provider::{self, ApiType, ModelRoute},
};
use serde_json::Value;
use std::time::Duration;
use tokio::time::{sleep, timeout};

type Https = HttpsConnector<HttpConnector>;
type HttpClient = Client<Https, Full<Bytes>>;

#[derive(Clone)]
pub struct UpstreamClient {
    client: HttpClient,
    max_bytes: usize,
    admission_wait: Duration,
}

pub struct Evaluation {
    pub response: JevResponse,
    pub upstream_request_id: Option<String>,
    pub provider: String,
    pub upstream_model: String,
    pub api_type: ApiType,
}

impl UpstreamClient {
    pub fn new(config: &AppConfig) -> Self {
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .enable_http2()
            .build();
        let mut builder = Client::builder(TokioExecutor::new());
        builder.pool_timer(TokioTimer::new());
        builder.pool_idle_timeout(config.pool_idle_timeout);
        builder.pool_max_idle_per_host(config.pool_max_idle_per_host);
        let client = builder.build::<_, Full<Bytes>>(https);
        Self {
            client,
            max_bytes: config.max_upstream_bytes,
            admission_wait: config.admission_wait,
        }
    }

    pub async fn evaluate(
        &self,
        req: &JevRequest,
        config: &AppConfig,
    ) -> Result<Evaluation, GatewayError> {
        let (alias, route, provider) = config.resolve(req.model.as_deref())?;
        let prepared = prepare_decision(req)?;
        let _permit = timeout(
            self.admission_wait,
            provider.permits.clone().acquire_owned(),
        )
        .await
        .map_err(|_| {
            GatewayError::local(
                StatusCode::SERVICE_UNAVAILABLE,
                "provider_busy",
                "upstream concurrency limit reached",
            )
        })?
        .map_err(|_| {
            GatewayError::local(
                StatusCode::SERVICE_UNAVAILABLE,
                "provider_unavailable",
                "provider admission controller closed",
            )
        })?;

        let reported_model = route.response_model.clone().unwrap_or(alias);
        let mut last_error: Option<GatewayError> = None;
        for attempt in 0..=provider.max_retries {
            // Mapping is part of the attempt, so wrong-shape output is a
            // retryable local `invalid_provider_response` like bad JSON (D7).
            let result = self.call_once(&provider, &route, &prepared).await.and_then(
                |(output, usage, request_id)| {
                    map_output(req, &prepared, &output, reported_model.clone(), usage)
                        .map(|response| (response, request_id))
                },
            );
            match result {
                Ok((response, request_id)) => {
                    return Ok(Evaluation {
                        response,
                        upstream_request_id: request_id,
                        provider: provider.name.clone(),
                        upstream_model: route.model.clone(),
                        api_type: provider.provider.api_type(),
                    });
                }
                Err(e)
                    if attempt < provider.max_retries
                        && retryable(provider.provider.api_type(), &e) =>
                {
                    last_error = Some(e);
                    sleep(Duration::from_millis(20 * (1u64 << attempt.min(3)))).await;
                }
                Err(e) => return Err(e),
            }
        }
        Err(last_error.unwrap_or_else(|| {
            GatewayError::upstream(
                StatusCode::BAD_GATEWAY,
                "provider_unavailable",
                "upstream request failed",
            )
        }))
    }

    async fn call_once(
        &self,
        provider: &ProviderRuntime,
        route: &ModelRoute,
        prepared: &PreparedDecision,
    ) -> Result<(Value, Option<Usage>, Option<String>), GatewayError> {
        timeout(
            provider.timeout,
            self.call_once_inner(provider, route, prepared),
        )
        .await
        .map_err(|_| {
            GatewayError::upstream(
                StatusCode::GATEWAY_TIMEOUT,
                "provider_timeout",
                format!(
                    "upstream {} deadline exceeded",
                    provider.provider.api_type().label()
                ),
            )
        })?
    }

    async fn call_once_inner(
        &self,
        provider: &ProviderRuntime,
        route: &ModelRoute,
        prepared: &PreparedDecision,
    ) -> Result<(Value, Option<Usage>, Option<String>), GatewayError> {
        let api = provider.provider.api_type().label();
        let upstream = provider::build_request(&provider.provider, route, prepared)?;
        let body = serde_json::to_vec(&upstream.body).map_err(|_| {
            GatewayError::local(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "failed to serialize upstream request",
            )
        })?;
        let mut builder = Request::builder()
            .method(Method::POST)
            .uri(provider.provider.uri.clone())
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json");
        for (k, v) in upstream.headers {
            builder = builder.header(k, v);
        }
        let request = builder.body(Full::new(Bytes::from(body))).map_err(|_| {
            GatewayError::local(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "failed to build upstream request",
            )
        })?;

        let mut response = self.client.request(request).await.map_err(|_| {
            GatewayError::upstream(
                StatusCode::BAD_GATEWAY,
                "provider_unavailable",
                format!("unable to reach upstream {api}"),
            )
        })?;
        let status = response.status();
        let retry_after = response
            .headers()
            .get(header::RETRY_AFTER)
            .and_then(|x| x.to_str().ok())
            .map(ToOwned::to_owned);
        let upstream_request_id = ["x-request-id", "request-id", "x-typesafe-request-id"]
            .iter()
            .find_map(|name| {
                response
                    .headers()
                    .get(*name)
                    .and_then(|x| x.to_str().ok())
                    .map(ToOwned::to_owned)
            });
        let bytes = read_bounded(response.body_mut(), self.max_bytes).await?;

        if !status.is_success() {
            return Err(upstream_status_error(api, status, retry_after));
        }
        let (output, usage) = provider::parse_response(&provider.provider, &bytes)?;
        Ok((output, usage, upstream_request_id))
    }
}

async fn read_bounded(
    body: &mut hyper::body::Incoming,
    limit: usize,
) -> Result<Bytes, GatewayError> {
    let mut out = BytesMut::with_capacity(limit.min(16 * 1024));
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| {
            GatewayError::upstream(
                StatusCode::BAD_GATEWAY,
                "provider_unavailable",
                "failed while reading upstream response",
            )
        })?;
        if let Ok(data) = frame.into_data() {
            if out.len().saturating_add(data.len()) > limit {
                return Err(GatewayError::local(
                    StatusCode::BAD_GATEWAY,
                    "invalid_provider_response",
                    format!("upstream response exceeded {limit} bytes"),
                ));
            }
            out.extend_from_slice(&data);
        }
    }
    Ok(out.freeze())
}

/// Maps an upstream non-2xx status to the caller-facing status and code (D19):
/// 429/529 keep their status as `provider_busy`, 5xx keep theirs as
/// `provider_unavailable`, and every other status becomes
/// `502 provider_rejected_request`, so an upstream 401/403/404 never blames the
/// caller's key or model.
fn upstream_error(status: StatusCode) -> (StatusCode, &'static str) {
    match status.as_u16() {
        429 | 529 => (status, "provider_busy"),
        500..=599 => (status, "provider_unavailable"),
        _ => (StatusCode::BAD_GATEWAY, "provider_rejected_request"),
    }
}

/// Upstream status only in the message; the upstream error body is never echoed.
fn upstream_status_error(
    api: &str,
    status: StatusCode,
    retry_after: Option<String>,
) -> GatewayError {
    let (mapped, code) = upstream_error(status);
    GatewayError::upstream(
        mapped,
        code,
        format!("upstream {api} returned HTTP {}", status.as_u16()),
    )
    .with_retry_after(retry_after)
}

fn retryable(api_type: ApiType, e: &GatewayError) -> bool {
    api_type.retryable_status(e.status.as_u16())
        && !matches!(
            e.code,
            "provider_refused" | "provider_output_truncated" | "provider_rejected_request"
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_error_table() {
        for status in [400, 401, 403, 404, 408, 413, 422] {
            let status = StatusCode::from_u16(status).unwrap();
            assert_eq!(
                upstream_error(status),
                (StatusCode::BAD_GATEWAY, "provider_rejected_request"),
                "{status}"
            );
        }
        for (status, code) in [
            (429, "provider_busy"),
            (529, "provider_busy"),
            (500, "provider_unavailable"),
            (503, "provider_unavailable"),
        ] {
            let status = StatusCode::from_u16(status).unwrap();
            assert_eq!(upstream_error(status), (status, code));
        }
    }

    #[test]
    fn upstream_status_error_forwards_retry_after_and_hides_body() {
        let e = upstream_status_error(
            "Messages API",
            StatusCode::TOO_MANY_REQUESTS,
            Some("7".into()),
        );
        assert_eq!(e.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(e.code, "provider_busy");
        assert_eq!(e.retry_after.as_deref(), Some("7"));
        assert_eq!(e.source, "upstream");

        let e = upstream_status_error("Responses API", StatusCode::NOT_FOUND, None);
        assert_eq!(e.status, StatusCode::BAD_GATEWAY);
        assert_eq!(e.code, "provider_rejected_request");
        assert_eq!(e.message, "upstream Responses API returned HTTP 404");
    }

    #[test]
    fn retryable_truth_table() {
        let statuses = [
            400, 401, 404, 408, 413, 422, 429, 500, 505, 502, 503, 504, 529,
        ];
        for api_type in [ApiType::ResponsesApi, ApiType::MessagesApi] {
            for status in statuses {
                let status = StatusCode::from_u16(status).unwrap();
                let expected = match status.as_u16() {
                    429 | 502 | 503 | 504 | 529 => true,
                    500 => api_type == ApiType::MessagesApi,
                    _ => false,
                };
                for code in [
                    "provider_busy",
                    "provider_unavailable",
                    "provider_timeout",
                    "invalid_provider_response",
                ] {
                    let e = GatewayError::upstream(status, code, "x");
                    assert_eq!(
                        retryable(api_type, &e),
                        expected,
                        "{api_type:?} {status} {code}"
                    );
                }
                for code in [
                    "provider_refused",
                    "provider_output_truncated",
                    "provider_rejected_request",
                ] {
                    let e = GatewayError::upstream(status, code, "x");
                    assert!(!retryable(api_type, &e), "{api_type:?} {status} {code}");
                }
            }
        }
        // Local invalid_provider_response (bad JSON, no text, oversize) stays
        // retryable (D7).
        let e = GatewayError::local(StatusCode::BAD_GATEWAY, "invalid_provider_response", "x");
        assert!(retryable(ApiType::ResponsesApi, &e));
        assert!(retryable(ApiType::MessagesApi, &e));
        // Every mapped upstream 4xx other than 429 is never retried.
        for api_type in [ApiType::ResponsesApi, ApiType::MessagesApi] {
            let e = upstream_status_error(api_type.label(), StatusCode::UNAUTHORIZED, None);
            assert!(!retryable(api_type, &e));
        }
    }
}
