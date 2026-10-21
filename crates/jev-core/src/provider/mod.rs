//! Provider runtime types and request/response dispatch by provider API type.
//!
//! The binary's upstream loop (admission, deadline, retries, bounded reads) is
//! shared by every provider; only [`build_request`] and [`parse_response`]
//! differ by [`ApiType`].

pub mod messages;
pub mod responses;

use crate::error::GatewayError;
use crate::model::{PreparedDecision, Usage};
use hyper::{
    header::{HeaderName, HeaderValue},
    Uri,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Upstream wire protocol of a provider, selected by the provider's `type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApiType {
    ResponsesApi,
    MessagesApi,
}

impl ApiType {
    /// Value of the `x-system-one-response-mode` success header.
    pub fn response_mode(self) -> &'static str {
        match self {
            ApiType::ResponsesApi => "responses-adapted",
            ApiType::MessagesApi => "messages-adapted",
        }
    }

    /// Human-readable upstream API name used in error messages.
    pub fn label(self) -> &'static str {
        match self {
            ApiType::ResponsesApi => "Responses API",
            ApiType::MessagesApi => "Messages API",
        }
    }

    /// Whether an attempt that failed with this HTTP status may be retried.
    /// Messages API also retries 500 (its documented transient `api_error`).
    /// The binary additionally excludes refusal, truncation and rejected-request
    /// codes, whatever their status.
    pub fn retryable_status(self, status: u16) -> bool {
        match self {
            ApiType::ResponsesApi => matches!(status, 429 | 502 | 503 | 504 | 529),
            ApiType::MessagesApi => matches!(status, 429 | 500 | 502 | 503 | 504 | 529),
        }
    }
}

/// A configured upstream provider. Secret values live only in the prebuilt
/// auth header; configuration names the environment variable that holds them.
#[derive(Debug)]
pub struct Provider {
    pub uri: Uri,
    /// Prebuilt authentication header, e.g. `Authorization: Bearer <key>`.
    pub auth: (HeaderName, HeaderValue),
    /// Validated custom headers; reserved names are rejected at startup.
    pub headers: Vec<(HeaderName, HeaderValue)>,
    pub settings: ProviderSettings,
}

impl Provider {
    pub fn api_type(&self) -> ApiType {
        match self.settings {
            ProviderSettings::Responses(_) => ApiType::ResponsesApi,
            ProviderSettings::Messages(_) => ApiType::MessagesApi,
        }
    }
}

/// Type-specific provider settings; the variant determines the [`ApiType`].
#[derive(Debug)]
pub enum ProviderSettings {
    Responses(ResponsesSettings),
    Messages(MessagesSettings),
}

#[derive(Debug)]
pub struct ResponsesSettings {
    pub store: Option<bool>,
    pub output_mode: ResponsesOutputMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponsesOutputMode {
    JsonSchema,
    JsonObject,
    Text,
}

#[derive(Debug)]
pub struct MessagesSettings {
    pub anthropic_version: HeaderValue,
    /// Comma-joined `anthropic-beta` value; `None` when no betas are configured.
    pub beta: Option<HeaderValue>,
    pub cache_system: bool,
    pub output_mode: MessagesOutputMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessagesOutputMode {
    JsonSchema,
    Prompt,
}

/// A public model alias resolved to a provider and upstream model ID.
#[derive(Debug, Clone)]
pub struct ModelRoute {
    pub provider: String,
    pub model: String,
    pub response_model: Option<String>,
    /// Generation settings; `Some` exactly when the provider is `messages-api`.
    pub messages: Option<MessagesRoute>,
    /// Optional reasoning settings; `None` unless the provider is
    /// `responses-api` and the route sets `effort` or `max_tokens`.
    pub responses: Option<ResponsesRoute>,
}

/// Responses API route settings. Unset fields are omitted from the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ResponsesRoute {
    /// Sent as `reasoning.effort`.
    pub effort: Option<ResponsesEffort>,
    /// Sent as `max_output_tokens` (reasoning plus visible output).
    pub max_output_tokens: Option<u32>,
}

/// Responses API `reasoning.effort` values; support varies by model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResponsesEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ResponsesEffort {
    pub fn as_str(self) -> &'static str {
        match self {
            ResponsesEffort::None => "none",
            ResponsesEffort::Minimal => "minimal",
            ResponsesEffort::Low => "low",
            ResponsesEffort::Medium => "medium",
            ResponsesEffort::High => "high",
            ResponsesEffort::Xhigh => "xhigh",
            ResponsesEffort::Max => "max",
        }
    }

    /// The Messages API counterpart; `None` for `none` and `minimal`.
    pub fn to_messages(self) -> Option<Effort> {
        match self {
            ResponsesEffort::None | ResponsesEffort::Minimal => None,
            ResponsesEffort::Low => Some(Effort::Low),
            ResponsesEffort::Medium => Some(Effort::Medium),
            ResponsesEffort::High => Some(Effort::High),
            ResponsesEffort::Xhigh => Some(Effort::Xhigh),
            ResponsesEffort::Max => Some(Effort::Max),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessagesRoute {
    pub thinking: Thinking,
    pub effort: Effort,
    pub max_tokens: u32,
}

impl Default for MessagesRoute {
    fn default() -> Self {
        Self {
            thinking: Thinking::Adaptive,
            effort: Effort::Medium,
            max_tokens: 16_000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Thinking {
    Adaptive,
    Disabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

/// Provider-specific request parts. The binary adds `content-type` and
/// `accept`, then these headers in order (auth, type-specific, custom).
#[derive(Debug)]
pub struct UpstreamRequest {
    pub headers: Vec<(HeaderName, HeaderValue)>,
    pub body: Value,
}

pub fn build_request(
    provider: &Provider,
    route: &ModelRoute,
    prepared: &PreparedDecision,
) -> Result<UpstreamRequest, GatewayError> {
    match &provider.settings {
        ProviderSettings::Responses(s) => responses::build_request(provider, s, route, prepared),
        ProviderSettings::Messages(s) => messages::build_request(provider, s, route, prepared),
    }
}

/// Parses a successful (2xx) upstream body into the structured output object
/// and usage. Non-2xx status mapping stays in the binary.
pub fn parse_response(
    provider: &Provider,
    body: &[u8],
) -> Result<(Value, Option<Usage>), GatewayError> {
    match &provider.settings {
        ProviderSettings::Responses(s) => responses::parse_response(s, body),
        ProviderSettings::Messages(s) => messages::parse_response(s, body),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::header;
    use serde_json::json;

    #[test]
    fn api_type_serde_uses_kebab_case() {
        assert_eq!(
            serde_json::from_value::<ApiType>(json!("responses-api")).unwrap(),
            ApiType::ResponsesApi
        );
        assert_eq!(
            serde_json::from_value::<ApiType>(json!("messages-api")).unwrap(),
            ApiType::MessagesApi
        );
        assert_eq!(
            serde_json::to_value(ApiType::MessagesApi).unwrap(),
            json!("messages-api")
        );
        assert!(serde_json::from_value::<ApiType>(json!("responses_api")).is_err());
        assert!(serde_json::from_value::<ApiType>(json!("openai")).is_err());
    }

    #[test]
    fn api_type_response_mode_header_values() {
        assert_eq!(ApiType::ResponsesApi.response_mode(), "responses-adapted");
        assert_eq!(ApiType::MessagesApi.response_mode(), "messages-adapted");
    }

    #[test]
    fn retryable_status_per_api_type() {
        for status in [
            400, 401, 403, 404, 408, 413, 422, 500, 505, 429, 502, 503, 504, 529,
        ] {
            let both = matches!(status, 429 | 502 | 503 | 504 | 529);
            assert_eq!(
                ApiType::ResponsesApi.retryable_status(status),
                both,
                "{status}"
            );
            assert_eq!(
                ApiType::MessagesApi.retryable_status(status),
                both || status == 500,
                "{status}"
            );
        }
    }

    #[test]
    fn dispatch_routes_messages_api_to_messages_mapping() {
        let provider = Provider {
            uri: Uri::from_static("https://api.anthropic.com/v1/messages"),
            auth: (
                HeaderName::from_static("x-api-key"),
                HeaderValue::from_static("test"),
            ),
            headers: vec![],
            settings: ProviderSettings::Messages(MessagesSettings {
                anthropic_version: HeaderValue::from_static("2023-06-01"),
                beta: None,
                cache_system: true,
                output_mode: MessagesOutputMode::JsonSchema,
            }),
        };
        assert_eq!(provider.api_type(), ApiType::MessagesApi);
        let (output, usage) = parse_response(
            &provider,
            br#"{"stop_reason":"end_turn","content":[{"type":"thinking","thinking":"","signature":"s"},{"type":"text","text":"{\"answers\":{}}"}],"usage":{"input_tokens":3,"cache_read_input_tokens":2,"output_tokens":4}}"#,
        )
        .unwrap();
        assert_eq!(output, json!({"answers": {}}));
        let usage = usage.unwrap();
        assert_eq!(usage.input_tokens, Some(5));
        assert_eq!(usage.output_tokens, Some(4));
    }

    #[test]
    fn dispatch_routes_responses_api_to_responses_mapping() {
        let provider = Provider {
            uri: Uri::from_static("https://api.openai.com/v1/responses"),
            auth: (
                header::AUTHORIZATION,
                HeaderValue::from_static("Bearer test"),
            ),
            headers: vec![],
            settings: ProviderSettings::Responses(ResponsesSettings {
                store: Some(false),
                output_mode: ResponsesOutputMode::JsonSchema,
            }),
        };
        assert_eq!(provider.api_type(), ApiType::ResponsesApi);
        let (output, usage) = parse_response(
            &provider,
            br#"{"output_text":"{\"answers\":{}}","usage":{"input_tokens":3,"output_tokens":4}}"#,
        )
        .unwrap();
        assert_eq!(output, json!({"answers": {}}));
        let usage = usage.unwrap();
        assert_eq!(usage.input_tokens, Some(3));
        assert_eq!(usage.output_tokens, Some(4));
    }
}
