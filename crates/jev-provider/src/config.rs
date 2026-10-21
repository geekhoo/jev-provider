use hyper::{
    header::{self, HeaderName, HeaderValue},
    StatusCode, Uri,
};
use jev_core::{
    error::GatewayError,
    provider::{
        ApiType, Effort, MessagesOutputMode, MessagesRoute, MessagesSettings, ModelRoute, Provider,
        ProviderSettings, ResponsesEffort, ResponsesOutputMode, ResponsesRoute, ResponsesSettings,
        Thinking,
    },
};
use serde::{Deserialize, Deserializer};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    env,
    str::FromStr,
    sync::Arc,
    time::Duration,
};
use tokio::sync::Semaphore;

#[derive(Debug)]
pub struct ProviderRuntime {
    pub name: String,
    pub provider: Provider,
    pub timeout: Duration,
    pub max_retries: u8,
    pub permits: Arc<Semaphore>,
}

/// One `GET /v1/models` entry, keyed by public alias. It never holds upstream
/// provider or model IDs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelInfo {
    pub description: String,
    pub release_date: String,
}

#[derive(Debug)]
pub struct AppConfig {
    pub bind: String,
    pub api_keys: HashSet<String>,
    pub providers: HashMap<String, Arc<ProviderRuntime>>,
    pub routes: HashMap<String, ModelRoute>,
    /// Model catalog for `/v1/models`, one entry per route alias, sorted by alias.
    pub models: BTreeMap<String, ModelInfo>,
    pub default_model: String,
    pub default_provider: String,
    pub allow_passthrough_model: bool,
    /// Sends `x-upstream-*` response headers, which name upstream providers and models.
    pub expose_upstream_headers: bool,
    pub max_body_bytes: usize,
    pub max_upstream_bytes: usize,
    pub max_questions: usize,
    pub max_choice_options: usize,
    pub max_score_levels: usize,
    pub max_json_depth: usize,
    /// Text units that make instructions, descriptions or `state` meaningful (P7).
    pub min_description_words: usize,
    pub admission_wait: Duration,
    pub pool_max_idle_per_host: usize,
    pub pool_idle_timeout: Duration,
    pub idempotency_ttl: Duration,
    pub idempotency_max_entries: usize,
    pub shutdown_grace: Duration,
    /// Bounds reading request headers and the request body.
    pub request_read_timeout: Duration,
}

/// One `PROVIDERS_JSON` entry. Every type-specific field is optional here so
/// that a field belonging to the other `type` is reported instead of ignored.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderConfigInput {
    #[serde(rename = "type")]
    api_type: ApiType,
    url: String,
    api_key_env: String,
    #[serde(default)]
    headers: HashMap<String, String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    max_retries: u8,
    #[serde(default)]
    max_in_flight: Option<usize>,
    #[serde(default)]
    output_mode: Option<OutputModeInput>,
    // responses-api only. `null` omits `store` from the request body.
    #[serde(default, deserialize_with = "present")]
    store: Option<Option<bool>>,
    // messages-api only.
    #[serde(default)]
    anthropic_version: Option<String>,
    #[serde(default)]
    betas: Option<Vec<String>>,
    #[serde(default)]
    auth: Option<AuthInput>,
    #[serde(default)]
    cache_system: Option<bool>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum OutputModeInput {
    JsonSchema,
    JsonObject,
    Text,
    Prompt,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AuthInput {
    #[default]
    XApiKey,
    Bearer,
}

/// One `MODEL_ROUTES_JSON` entry.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RouteInput {
    provider: String,
    model: String,
    #[serde(default)]
    response_model: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    release_date: Option<String>,
    // Both provider types; messages-api rejects `none` and `minimal`.
    #[serde(default)]
    effort: Option<ResponsesEffort>,
    // messages-api only.
    #[serde(default)]
    thinking: Option<Thinking>,
    // Both provider types; responses-api sends it as `max_output_tokens`.
    #[serde(default)]
    max_tokens: Option<u32>,
}

/// Distinguishes an explicit `null` (`Some(None)`) from an absent field (`None`).
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

const DEFAULT_ANTHROPIC_VERSION: &str = "2023-06-01";
const MAX_ROUTE_MAX_TOKENS: u32 = 128_000;
/// `/v1/models` release date for routes without `release_date`: the release
/// date of this gateway version.
pub const DEFAULT_RELEASE_DATE: &str = "2026-10-13";

impl AppConfig {
    pub fn from_env() -> Result<Self, GatewayError> {
        Self::from_lookup(&|name| env::var(name).ok())
    }

    /// Reads configuration through `var` so tests stay hermetic. Empty or
    /// whitespace-only values count as unset. Errors name environment variables,
    /// never their values.
    pub fn from_lookup(var: &dyn Fn(&str) -> Option<String>) -> Result<Self, GatewayError> {
        let non_empty = |name: &str| var(name).filter(|v| !v.trim().is_empty());
        let var: &dyn Fn(&str) -> Option<String> = &non_empty;
        let bind = var("BIND_ADDR").unwrap_or_else(|| "0.0.0.0:8080".to_owned());
        let default_model = var("DEFAULT_JEV_MODEL").unwrap_or_else(|| "jev-latest".to_owned());
        let allow_passthrough_model = parse_env(var, "ALLOW_PASSTHROUGH_MODEL", false);
        let expose_upstream_headers = parse_env(var, "EXPOSE_UPSTREAM_HEADERS", false);
        let max_body_bytes = parse_env(var, "MAX_BODY_BYTES", 524_288);
        let max_upstream_bytes = parse_env(var, "MAX_UPSTREAM_BYTES", 1_048_576);
        let max_questions = parse_env(var, "MAX_QUESTIONS", 32);
        let max_choice_options = parse_env(var, "MAX_CHOICE_OPTIONS", 255);
        let max_score_levels = parse_env(var, "MAX_SCORE_LEVELS", 10);
        let max_json_depth = parse_env(var, "MAX_JSON_DEPTH", 32);
        // Parsed as a signed number so that 0 and negative values are rejected.
        let min_description_words = parse_env::<i64>(var, "MIN_DESCRIPTION_WORDS", 3);
        if min_description_words < 1 {
            return Err(config_err("MIN_DESCRIPTION_WORDS must be at least 1"));
        }
        let min_description_words = usize::try_from(min_description_words)
            .map_err(|_| config_err("MIN_DESCRIPTION_WORDS is too large"))?;
        let admission_wait = Duration::from_millis(parse_env(var, "ADMISSION_WAIT_MS", 5));
        let global_max_in_flight = parse_env::<usize>(var, "MAX_IN_FLIGHT", 1024).max(1);
        let pool_max_idle_per_host = parse_env::<usize>(var, "POOL_MAX_IDLE_PER_HOST", 256).max(1);
        let pool_idle_timeout =
            Duration::from_millis(parse_env(var, "POOL_IDLE_TIMEOUT_MS", 90_000));
        let idempotency_ttl =
            Duration::from_secs(parse_env(var, "IDEMPOTENCY_TTL_SECONDS", 86_400));
        let idempotency_max_entries =
            parse_env::<usize>(var, "IDEMPOTENCY_MAX_ENTRIES", 100_000).max(64);
        let shutdown_grace = Duration::from_millis(parse_env(var, "SHUTDOWN_GRACE_MS", 10_000));
        let request_read_timeout_ms = parse_env::<u64>(var, "REQUEST_READ_TIMEOUT_MS", 10_000);
        if request_read_timeout_ms < 100 {
            return Err(config_err("REQUEST_READ_TIMEOUT_MS must be at least 100"));
        }
        let request_read_timeout = Duration::from_millis(request_read_timeout_ms);
        let api_keys = var("GATEWAY_API_KEYS")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|x| !x.is_empty())
            .map(ToOwned::to_owned)
            .collect();

        let providers_input: HashMap<String, ProviderConfigInput> = match var("PROVIDERS_JSON") {
            Some(s) => serde_json::from_str(&s)
                .map_err(|e| config_err(format!("invalid PROVIDERS_JSON: {e}")))?,
            None => builtin_providers(var),
        };
        if providers_input.is_empty() {
            return Err(config_err(
                "no provider is configured: set OPENAI_API_KEY and/or ANTHROPIC_API_KEY, or PROVIDERS_JSON",
            ));
        }

        let mut providers = HashMap::with_capacity(providers_input.len());
        for (name, p) in providers_input {
            let runtime = build_provider(&name, p, global_max_in_flight, var)?;
            providers.insert(name, Arc::new(runtime));
        }

        let routes_input: Option<HashMap<String, RouteInput>> = match var("MODEL_ROUTES_JSON") {
            Some(s) => Some(
                serde_json::from_str(&s)
                    .map_err(|e| config_err(format!("invalid MODEL_ROUTES_JSON: {e}")))?,
            ),
            None => None,
        };

        let default_provider = match var("DEFAULT_PROVIDER") {
            Some(name) if providers.contains_key(&name) => name,
            Some(name) => {
                return Err(config_err(format!(
                    "DEFAULT_PROVIDER '{name}' is not a configured provider; configured: {}",
                    provider_names(&providers)
                )))
            }
            None if providers.len() == 1 => providers.keys().next().cloned().unwrap_or_default(),
            // MODEL_ROUTES_JSON may choose the default through the default alias's route.
            None => match routes_input.as_ref().and_then(|r| r.get(&default_model)) {
                Some(route) => route.provider.clone(),
                None => {
                    return Err(config_err(format!(
                        "multiple providers are configured ({}); set DEFAULT_PROVIDER to one of them \
                         (and its model with OPENAI_MODEL or ANTHROPIC_MODEL), or set MODEL_ROUTES_JSON \
                         with a route for the default alias '{default_model}'",
                        provider_names(&providers)
                    )))
                }
            },
        };

        let routes_input = match routes_input {
            Some(r) => r,
            None => {
                let api_type = providers[&default_provider].provider.api_type();
                HashMap::from([(
                    default_model.clone(),
                    RouteInput {
                        provider: default_provider.clone(),
                        model: default_route_model(api_type, var),
                        response_model: None,
                        description: None,
                        release_date: None,
                        effort: None,
                        thinking: None,
                        max_tokens: None,
                    },
                )])
            }
        };
        let mut routes = HashMap::with_capacity(routes_input.len());
        let mut models = BTreeMap::new();
        for (alias, mut r) in routes_input {
            let release_date = r
                .release_date
                .take()
                .unwrap_or_else(|| DEFAULT_RELEASE_DATE.to_owned());
            if !is_iso_date(&release_date) {
                return Err(config_err(format!(
                    "route {alias}: release_date must be a valid YYYY-MM-DD date"
                )));
            }
            let description = r.description.take().unwrap_or_default();
            let route = build_route(&alias, r, &providers)?;
            models.insert(
                alias.clone(),
                ModelInfo {
                    description,
                    release_date,
                },
            );
            routes.insert(alias, route);
        }

        Ok(Self {
            bind,
            api_keys,
            providers,
            routes,
            models,
            default_model,
            default_provider,
            allow_passthrough_model,
            expose_upstream_headers,
            max_body_bytes,
            max_upstream_bytes,
            max_questions,
            max_choice_options,
            max_score_levels,
            max_json_depth,
            min_description_words,
            admission_wait,
            pool_max_idle_per_host,
            pool_idle_timeout,
            idempotency_ttl,
            idempotency_max_entries,
            shutdown_grace,
            request_read_timeout,
        })
    }

    pub fn resolve(
        &self,
        alias: Option<&str>,
    ) -> Result<(String, ModelRoute, Arc<ProviderRuntime>), GatewayError> {
        let alias = alias.unwrap_or(&self.default_model).to_owned();
        let route = if let Some(r) = self.routes.get(&alias) {
            r.clone()
        } else if self.allow_passthrough_model {
            // Passthrough models use the default provider and its type defaults.
            ModelRoute {
                provider: self.default_provider.clone(),
                model: alias.clone(),
                response_model: None,
                messages: self
                    .providers
                    .get(&self.default_provider)
                    .filter(|p| p.provider.api_type() == ApiType::MessagesApi)
                    .map(|_| MessagesRoute::default()),
                responses: None,
            }
        } else {
            return Err(GatewayError::local(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_request",
                format!("unknown model '{alias}'"),
            ));
        };
        let provider = self
            .providers
            .get(&route.provider)
            .cloned()
            .ok_or_else(|| {
                GatewayError::local(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "provider_not_configured",
                    format!("provider '{}' is not configured", route.provider),
                )
            })?;
        Ok((alias, route, provider))
    }
}

/// The registry used when `PROVIDERS_JSON` is unset: one provider per present key.
fn builtin_providers(var: &dyn Fn(&str) -> Option<String>) -> HashMap<String, ProviderConfigInput> {
    let builtin = |api_type, url: &str, api_key_env: &str| ProviderConfigInput {
        api_type,
        url: url.to_owned(),
        api_key_env: api_key_env.to_owned(),
        headers: HashMap::new(),
        timeout_ms: None,
        max_retries: 0,
        max_in_flight: None,
        output_mode: None,
        store: None,
        anthropic_version: None,
        betas: None,
        auth: None,
        cache_system: None,
    };
    let mut providers = HashMap::new();
    if var("OPENAI_API_KEY").is_some() {
        providers.insert(
            "openai".to_owned(),
            builtin(
                ApiType::ResponsesApi,
                "https://api.openai.com/v1/responses",
                "OPENAI_API_KEY",
            ),
        );
    }
    if var("ANTHROPIC_API_KEY").is_some() {
        providers.insert(
            "anthropic".to_owned(),
            builtin(
                ApiType::MessagesApi,
                "https://api.anthropic.com/v1/messages",
                "ANTHROPIC_API_KEY",
            ),
        );
    }
    providers
}

/// Upstream model of the generated default route, by the default provider's type.
fn default_route_model(api_type: ApiType, var: &dyn Fn(&str) -> Option<String>) -> String {
    let (name, default) = match api_type {
        ApiType::ResponsesApi => ("OPENAI_MODEL", "gpt-6-astra"),
        ApiType::MessagesApi => ("ANTHROPIC_MODEL", "claude-haiku-5-5"),
    };
    var(name).unwrap_or_else(|| default.to_owned())
}

fn provider_names(providers: &HashMap<String, Arc<ProviderRuntime>>) -> String {
    let mut names: Vec<&str> = providers.keys().map(String::as_str).collect();
    names.sort_unstable();
    names.join(", ")
}

fn build_provider(
    name: &str,
    p: ProviderConfigInput,
    global_max_in_flight: usize,
    var: &dyn Fn(&str) -> Option<String>,
) -> Result<ProviderRuntime, GatewayError> {
    let uri = Uri::from_str(&p.url).map_err(|e| config_err(format!("provider {name} url: {e}")))?;
    if uri.scheme_str() != Some("https") && uri.scheme_str() != Some("http") {
        return Err(config_err(format!(
            "provider {name} URL must be http or https"
        )));
    }
    let key = var(&p.api_key_env).ok_or_else(|| {
        config_err(format!(
            "provider {name}: environment variable {} is missing",
            p.api_key_env
        ))
    })?;
    let header_value = |value: String, what: &str| {
        HeaderValue::from_str(&value)
            .map_err(|_| config_err(format!("provider {name}: invalid {what} header")))
    };

    let (auth, settings, default_timeout_ms) = match p.api_type {
        ApiType::ResponsesApi => {
            reject_fields(
                name,
                p.api_type,
                &[
                    ("anthropic_version", p.anthropic_version.is_some()),
                    ("betas", p.betas.is_some()),
                    ("auth", p.auth.is_some()),
                    ("cache_system", p.cache_system.is_some()),
                ],
            )?;
            let output_mode = match p.output_mode {
                None | Some(OutputModeInput::JsonSchema) => ResponsesOutputMode::JsonSchema,
                Some(OutputModeInput::JsonObject) => ResponsesOutputMode::JsonObject,
                Some(OutputModeInput::Text) => ResponsesOutputMode::Text,
                Some(OutputModeInput::Prompt) => {
                    return Err(config_err(format!(
                        "provider {name}: output_mode prompt is not valid for type responses-api"
                    )))
                }
            };
            let auth = (
                header::AUTHORIZATION,
                header_value(format!("Bearer {key}"), "API key")?,
            );
            let settings = ProviderSettings::Responses(ResponsesSettings {
                store: p.store.unwrap_or(Some(false)),
                output_mode,
            });
            (auth, settings, 20_000)
        }
        ApiType::MessagesApi => {
            reject_fields(name, p.api_type, &[("store", p.store.is_some())])?;
            let output_mode = match p.output_mode {
                None | Some(OutputModeInput::JsonSchema) => MessagesOutputMode::JsonSchema,
                Some(OutputModeInput::Prompt) => MessagesOutputMode::Prompt,
                Some(OutputModeInput::JsonObject | OutputModeInput::Text) => {
                    return Err(config_err(format!(
                        "provider {name}: output_mode must be json_schema or prompt for type messages-api"
                    )))
                }
            };
            let auth = match p.auth.unwrap_or_default() {
                AuthInput::XApiKey => (
                    HeaderName::from_static("x-api-key"),
                    header_value(key, "API key")?,
                ),
                AuthInput::Bearer => (
                    header::AUTHORIZATION,
                    header_value(format!("Bearer {key}"), "API key")?,
                ),
            };
            let anthropic_version = header_value(
                p.anthropic_version
                    .unwrap_or_else(|| DEFAULT_ANTHROPIC_VERSION.to_owned()),
                "anthropic-version",
            )?;
            let betas = p.betas.unwrap_or_default();
            if betas.iter().any(|b| b.trim().is_empty() || b.contains(',')) {
                return Err(config_err(format!(
                    "provider {name}: betas entries must be non-blank and contain no commas"
                )));
            }
            let beta = if betas.is_empty() {
                None
            } else {
                Some(header_value(betas.join(","), "anthropic-beta")?)
            };
            let settings = ProviderSettings::Messages(MessagesSettings {
                anthropic_version,
                beta,
                cache_system: p.cache_system.unwrap_or(true),
                output_mode,
            });
            (auth, settings, 60_000)
        }
    };

    let mut headers = Vec::with_capacity(p.headers.len());
    for (k, v) in p.headers {
        let hn = HeaderName::from_str(&k)
            .map_err(|_| config_err(format!("provider {name}: invalid header name {k}")))?;
        let reserved = matches!(
            hn.as_str(),
            "authorization" | "content-type" | "content-length" | "host"
        ) || (p.api_type == ApiType::MessagesApi
            && matches!(
                hn.as_str(),
                "x-api-key" | "anthropic-version" | "anthropic-beta"
            ));
        if reserved {
            return Err(config_err(format!(
                "provider {name}: header {k} is reserved"
            )));
        }
        let hv = HeaderValue::from_str(&v)
            .map_err(|_| config_err(format!("provider {name}: invalid value for header {k}")))?;
        headers.push((hn, hv));
    }

    Ok(ProviderRuntime {
        name: name.to_owned(),
        provider: Provider {
            uri,
            auth,
            headers,
            settings,
        },
        timeout: Duration::from_millis(p.timeout_ms.unwrap_or(default_timeout_ms).max(100)),
        max_retries: p.max_retries.min(3),
        permits: Arc::new(Semaphore::new(
            p.max_in_flight.unwrap_or(global_max_in_flight).max(1),
        )),
    })
}

fn build_route(
    alias: &str,
    r: RouteInput,
    providers: &HashMap<String, Arc<ProviderRuntime>>,
) -> Result<ModelRoute, GatewayError> {
    let provider = providers.get(&r.provider).ok_or_else(|| {
        config_err(format!(
            "route {alias}: provider '{}' is not configured",
            r.provider
        ))
    })?;
    let max_tokens_err = || {
        config_err(format!(
            "route {alias}: max_tokens must be 1..={MAX_ROUTE_MAX_TOKENS}"
        ))
    };
    let (messages, responses) = match provider.provider.api_type() {
        ApiType::ResponsesApi => {
            if r.thinking.is_some() {
                return Err(config_err(format!(
                    "route {alias}: thinking applies only to messages-api providers"
                )));
            }
            if r.max_tokens
                .is_some_and(|m| !(1..=MAX_ROUTE_MAX_TOKENS).contains(&m))
            {
                return Err(max_tokens_err());
            }
            let responses =
                (r.effort.is_some() || r.max_tokens.is_some()).then_some(ResponsesRoute {
                    effort: r.effort,
                    max_output_tokens: r.max_tokens,
                });
            (None, responses)
        }
        ApiType::MessagesApi => {
            let effort = match r.effort {
                None => None,
                Some(e) => Some(e.to_messages().ok_or_else(|| {
                    config_err(format!(
                        "route {alias}: effort {} applies only to responses-api providers; messages-api accepts low|medium|high|xhigh|max",
                        e.as_str()
                    ))
                })?),
            };
            let defaults = MessagesRoute::default();
            let m = MessagesRoute {
                thinking: r.thinking.unwrap_or(defaults.thinking),
                effort: effort.unwrap_or(defaults.effort),
                max_tokens: r.max_tokens.unwrap_or(defaults.max_tokens),
            };
            if m.thinking == Thinking::Disabled && matches!(m.effort, Effort::Xhigh | Effort::Max) {
                return Err(config_err(format!(
                    "route {alias}: thinking disabled cannot be combined with effort xhigh or max"
                )));
            }
            if !(1..=MAX_ROUTE_MAX_TOKENS).contains(&m.max_tokens) {
                return Err(max_tokens_err());
            }
            (Some(m), None)
        }
    };
    Ok(ModelRoute {
        provider: r.provider,
        model: r.model,
        response_model: r.response_model,
        messages,
        responses,
    })
}

fn reject_fields(
    name: &str,
    api_type: ApiType,
    fields: &[(&str, bool)],
) -> Result<(), GatewayError> {
    if let Some((field, _)) = fields.iter().find(|(_, set)| *set) {
        let type_name = match api_type {
            ApiType::ResponsesApi => "responses-api",
            ApiType::MessagesApi => "messages-api",
        };
        return Err(config_err(format!(
            "provider {name}: field {field} is not valid for type {type_name}"
        )));
    }
    Ok(())
}

/// Accepts only `YYYY-MM-DD` naming a real Gregorian calendar day.
fn is_iso_date(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return false;
    }
    let num = |r: std::ops::Range<usize>| {
        b[r.clone()]
            .iter()
            .all(u8::is_ascii_digit)
            .then(|| s[r].parse::<u32>().ok())
            .flatten()
    };
    let (Some(year), Some(month), Some(day)) = (num(0..4), num(5..7), num(8..10)) else {
        return false;
    };
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    (1..=days).contains(&day)
}

fn parse_env<T: FromStr>(var: &dyn Fn(&str) -> Option<String>, name: &str, default: T) -> T {
    var(name).and_then(|x| x.parse().ok()).unwrap_or(default)
}
fn config_err(message: impl Into<String>) -> GatewayError {
    GatewayError::local(
        StatusCode::INTERNAL_SERVER_ERROR,
        "provider_configuration_error",
        message,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(vars: &[(&str, &str)]) -> Result<AppConfig, GatewayError> {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        AppConfig::from_lookup(&|name| vars.get(name).cloned())
    }

    fn err_message(r: Result<AppConfig, GatewayError>) -> String {
        let e = r.unwrap_err();
        assert_eq!(e.code, "provider_configuration_error");
        e.message
    }

    const PROVIDERS: &str = r#"{
        "openai": {"type":"responses-api","url":"https://api.openai.com/v1/responses","api_key_env":"OPENAI_API_KEY",
                   "headers":{"X-App":"jev"},"output_mode":"json_object","timeout_ms":15000,"max_retries":9,"max_in_flight":7},
        "anthropic": {"type":"messages-api","url":"https://api.anthropic.com/v1/messages","api_key_env":"ANTHROPIC_API_KEY",
                      "betas":["a-2026-01-01","b-2026-02-02"],"cache_system":false,"output_mode":"prompt"},
        "gateway": {"type":"messages-api","url":"https://llm.example.com/v1/messages","api_key_env":"GATEWAY_KEY",
                    "auth":"bearer","anthropic_version":"2023-07-01"}
    }"#;

    const KEYS: [(&str, &str); 3] = [
        ("OPENAI_API_KEY", "sk-o"),
        ("ANTHROPIC_API_KEY", "sk-a"),
        ("GATEWAY_KEY", "sk-g"),
    ];

    fn with_providers(providers: &str, routes: &str) -> Result<AppConfig, GatewayError> {
        let mut vars = KEYS.to_vec();
        vars.push(("PROVIDERS_JSON", providers));
        vars.push(("MODEL_ROUTES_JSON", routes));
        config(&vars)
    }

    #[test]
    fn min_description_words_defaults_to_three_and_rejects_zero() {
        let words = |value: Option<&str>| {
            let mut vars = vec![("ANTHROPIC_API_KEY", "sk-a")];
            if let Some(v) = value {
                vars.push(("MIN_DESCRIPTION_WORDS", v));
            }
            config(&vars)
        };
        assert_eq!(words(None).unwrap().min_description_words, 3);
        assert_eq!(words(Some("1")).unwrap().min_description_words, 1);
        assert_eq!(words(Some("5")).unwrap().min_description_words, 5);
        for bad in ["0", "-1"] {
            assert!(err_message(words(Some(bad))).contains("MIN_DESCRIPTION_WORDS"));
        }
        // Unparseable values fall back to the default, like every parse_env setting.
        assert_eq!(words(Some("many")).unwrap().min_description_words, 3);
    }

    #[test]
    fn default_config_is_anthropic_haiku_adaptive() {
        let c = config(&[("ANTHROPIC_API_KEY", "sk-a")]).unwrap();
        assert_eq!(c.default_model, "jev-latest");
        assert_eq!(c.default_provider, "anthropic");
        let p = &c.providers["anthropic"];
        assert_eq!(p.provider.api_type(), ApiType::MessagesApi);
        assert_eq!(p.provider.uri, "https://api.anthropic.com/v1/messages");
        assert_eq!(p.timeout, Duration::from_millis(60_000));
        assert_eq!(p.provider.auth.0, "x-api-key");
        assert_eq!(p.provider.auth.1, "sk-a");
        let ProviderSettings::Messages(s) = &p.provider.settings else {
            panic!("expected messages-api settings");
        };
        assert_eq!(s.anthropic_version, "2023-06-01");
        assert!(s.beta.is_none());
        assert!(s.cache_system);
        assert_eq!(s.output_mode, MessagesOutputMode::JsonSchema);

        let (alias, route, provider) = c.resolve(None).unwrap();
        assert_eq!(alias, "jev-latest");
        assert_eq!(route.provider, "anthropic");
        assert_eq!(route.model, "claude-haiku-5-5");
        assert_eq!(provider.name, "anthropic");
        assert_eq!(
            route.messages,
            Some(MessagesRoute {
                thinking: Thinking::Adaptive,
                effort: Effort::Medium,
                max_tokens: 16_000,
            })
        );
    }

    fn default_route(c: &AppConfig) -> (String, String) {
        let (_, route, provider) = c.resolve(None).unwrap();
        assert_eq!(route.provider, c.default_provider);
        (provider.name.clone(), route.model)
    }

    const BOTH_KEYS: [(&str, &str); 2] =
        [("OPENAI_API_KEY", "sk-o"), ("ANTHROPIC_API_KEY", "sk-a")];

    #[test]
    fn openai_only_defaults_to_responses_api() {
        let c = config(&[("OPENAI_API_KEY", "sk-o")]).unwrap();
        assert_eq!(c.providers.len(), 1);
        assert_eq!(c.default_provider, "openai");
        let p = &c.providers["openai"];
        assert_eq!(p.provider.api_type(), ApiType::ResponsesApi);
        assert_eq!(p.provider.uri, "https://api.openai.com/v1/responses");
        assert_eq!(p.provider.auth.1, "Bearer sk-o");
        assert_eq!(p.timeout, Duration::from_millis(20_000));
        let ProviderSettings::Responses(s) = &p.provider.settings else {
            panic!("expected responses-api settings");
        };
        assert_eq!(s.store, Some(false));
        assert_eq!(s.output_mode, ResponsesOutputMode::JsonSchema);
        assert_eq!(default_route(&c), ("openai".into(), "gpt-6-astra".into()));
        assert!(c.routes["jev-latest"].messages.is_none());

        let c = config(&[("OPENAI_API_KEY", "sk-o"), ("OPENAI_MODEL", "gpt-x")]).unwrap();
        assert_eq!(default_route(&c), ("openai".into(), "gpt-x".into()));
    }

    #[test]
    fn anthropic_only_ignores_openai_model_and_accepts_matching_default_provider() {
        let c = config(&[
            ("ANTHROPIC_API_KEY", "sk-a"),
            ("ANTHROPIC_MODEL", "claude-sonnet-5-5"),
            ("OPENAI_MODEL", "gpt-x"),
            ("DEFAULT_PROVIDER", "anthropic"),
            ("DEFAULT_JEV_MODEL", "jev-x"),
        ])
        .unwrap();
        assert_eq!(c.providers.len(), 1);
        assert_eq!(c.default_model, "jev-x");
        assert_eq!(
            default_route(&c),
            ("anthropic".into(), "claude-sonnet-5-5".into())
        );
    }

    #[test]
    fn both_keys_require_default_provider() {
        let message = err_message(config(&BOTH_KEYS));
        assert!(message.contains("multiple providers are configured (anthropic, openai)"));
        for hint in [
            "DEFAULT_PROVIDER",
            "OPENAI_MODEL",
            "ANTHROPIC_MODEL",
            "MODEL_ROUTES_JSON",
        ] {
            assert!(message.contains(hint), "{hint}: {message}");
        }
        assert!(!message.contains("sk-"), "{message}");
    }

    #[test]
    fn both_keys_with_default_provider_choose_its_model() {
        let mut vars = BOTH_KEYS.to_vec();
        vars.extend([("OPENAI_MODEL", "gpt-x"), ("ANTHROPIC_MODEL", "claude-x")]);
        for (choice, model) in [("openai", "gpt-x"), ("anthropic", "claude-x")] {
            let mut vars = vars.clone();
            vars.push(("DEFAULT_PROVIDER", choice));
            let c = config(&vars).unwrap();
            assert_eq!(c.providers.len(), 2);
            assert_eq!(c.default_provider, choice);
            assert_eq!(default_route(&c), (choice.into(), model.into()));
        }
        // Without a *_MODEL override the chosen provider's default model is used.
        let mut vars = BOTH_KEYS.to_vec();
        vars.push(("DEFAULT_PROVIDER", "openai"));
        assert_eq!(
            default_route(&config(&vars).unwrap()),
            ("openai".into(), "gpt-6-astra".into())
        );
    }

    #[test]
    fn no_provider_is_an_error() {
        for vars in [
            &[][..],
            &[("OPENAI_API_KEY", ""), ("ANTHROPIC_API_KEY", "  ")][..],
            &[("PROVIDERS_JSON", "{}")][..],
        ] {
            let message = err_message(config(vars));
            for name in ["OPENAI_API_KEY", "ANTHROPIC_API_KEY", "PROVIDERS_JSON"] {
                assert!(message.contains(name), "{name}: {message}");
            }
        }
    }

    #[test]
    fn empty_key_counts_as_unset() {
        let c = config(&[("OPENAI_API_KEY", ""), ("ANTHROPIC_API_KEY", "sk-a")]).unwrap();
        assert_eq!(c.providers.len(), 1);
        assert_eq!(c.default_provider, "anthropic");
        // An empty DEFAULT_PROVIDER is unset too.
        let mut vars = BOTH_KEYS.to_vec();
        vars.push(("DEFAULT_PROVIDER", ""));
        assert!(err_message(config(&vars)).contains("multiple providers"));
        // A registry key that is empty is reported as missing.
        let message = err_message(config(&[
            (
                "PROVIDERS_JSON",
                r#"{"openai":{"type":"responses-api","url":"https://x.test/v1","api_key_env":"OPENAI_API_KEY"}}"#,
            ),
            ("OPENAI_API_KEY", ""),
        ]));
        assert!(message.contains("OPENAI_API_KEY is missing"), "{message}");
    }

    #[test]
    fn unknown_default_provider_is_an_error() {
        for vars in [
            vec![
                ("ANTHROPIC_API_KEY", "sk-a"),
                ("DEFAULT_PROVIDER", "anthropc"),
            ],
            vec![
                ("OPENAI_API_KEY", "sk-o"),
                ("DEFAULT_PROVIDER", "anthropic"),
            ],
            vec![
                ("OPENAI_API_KEY", "sk-o"),
                ("ANTHROPIC_API_KEY", "sk-a"),
                ("DEFAULT_PROVIDER", "OpenAI"),
            ],
        ] {
            let message = err_message(config(&vars));
            assert!(
                message.contains("is not a configured provider"),
                "{message}"
            );
            assert!(message.contains("configured: "), "{message}");
            assert!(!message.contains("sk-"), "{message}");
        }
    }

    #[test]
    fn registry_with_several_providers_requires_a_default() {
        let mut vars = KEYS.to_vec();
        vars.push(("PROVIDERS_JSON", PROVIDERS));
        let message = err_message(config(&vars));
        assert!(
            message.contains("multiple providers are configured (anthropic, gateway, openai)"),
            "{message}"
        );

        // DEFAULT_PROVIDER picks the model env var by the provider's type.
        vars.extend([
            ("DEFAULT_PROVIDER", "gateway"),
            ("ANTHROPIC_MODEL", "claude-x"),
        ]);
        let c = config(&vars).unwrap();
        assert_eq!(default_route(&c), ("gateway".into(), "claude-x".into()));
        assert!(c.routes["jev-latest"].messages.is_some());

        // A MODEL_ROUTES_JSON route for the default alias also chooses it.
        let c = with_providers(
            PROVIDERS,
            r#"{"jev-latest":{"provider":"openai","model":"gpt-6-astra"}}"#,
        )
        .unwrap();
        assert_eq!(c.default_provider, "openai");
        let message = err_message(with_providers(
            PROVIDERS,
            r#"{"jev-other":{"provider":"openai","model":"gpt-6-astra"}}"#,
        ));
        assert!(message.contains("default alias 'jev-latest'"), "{message}");
    }

    #[test]
    fn parses_registry_for_both_types() {
        let c = with_providers(
            PROVIDERS,
            r#"{"jev-latest":{"provider":"openai","model":"gpt-6-astra"},
                "jev-claude":{"provider":"anthropic","model":"claude-haiku-5-5","effort":"high","max_tokens":32000}}"#,
        )
        .unwrap();

        let o = &c.providers["openai"];
        assert_eq!(o.provider.api_type(), ApiType::ResponsesApi);
        assert_eq!(o.provider.auth.0, header::AUTHORIZATION);
        assert_eq!(o.provider.auth.1, "Bearer sk-o");
        assert_eq!(o.provider.headers[0].0, "x-app");
        assert_eq!(o.timeout, Duration::from_millis(15_000));
        assert_eq!(o.max_retries, 3);
        assert_eq!(o.permits.available_permits(), 7);
        let ProviderSettings::Responses(s) = &o.provider.settings else {
            panic!("expected responses-api settings");
        };
        assert_eq!(s.store, Some(false));
        assert_eq!(s.output_mode, ResponsesOutputMode::JsonObject);

        let a = &c.providers["anthropic"];
        assert_eq!(a.provider.auth.0, "x-api-key");
        assert_eq!(a.timeout, Duration::from_millis(60_000));
        let ProviderSettings::Messages(s) = &a.provider.settings else {
            panic!("expected messages-api settings");
        };
        assert_eq!(s.beta.as_ref().unwrap(), "a-2026-01-01,b-2026-02-02");
        assert!(!s.cache_system);
        assert_eq!(s.output_mode, MessagesOutputMode::Prompt);

        let g = &c.providers["gateway"];
        assert_eq!(g.provider.auth.0, header::AUTHORIZATION);
        assert_eq!(g.provider.auth.1, "Bearer sk-g");
        let ProviderSettings::Messages(s) = &g.provider.settings else {
            panic!("expected messages-api settings");
        };
        assert_eq!(s.anthropic_version, "2023-07-01");

        assert!(c.routes["jev-latest"].messages.is_none());
        assert!(c.routes["jev-latest"].responses.is_none());
        assert!(c.routes["jev-claude"].responses.is_none());
        assert_eq!(
            c.routes["jev-claude"].messages,
            Some(MessagesRoute {
                thinking: Thinking::Adaptive,
                effort: Effort::High,
                max_tokens: 32_000,
            })
        );
    }

    #[test]
    fn responses_store_null_is_omitted() {
        let c = with_providers(
            r#"{"openai":{"type":"responses-api","url":"https://x.test/v1/responses","api_key_env":"OPENAI_API_KEY","store":null}}"#,
            r#"{}"#,
        )
        .unwrap();
        let ProviderSettings::Responses(s) = &c.providers["openai"].provider.settings else {
            panic!("expected responses-api settings");
        };
        assert_eq!(s.store, None);
        assert_eq!(c.providers["openai"].timeout, Duration::from_millis(20_000));
    }

    #[test]
    fn rejects_cross_type_and_unknown_provider_fields() {
        let cases = [
            (
                r#""type":"responses-api","anthropic_version":"2023-06-01""#,
                "field anthropic_version is not valid for type responses-api",
            ),
            (
                r#""type":"responses-api","betas":[]"#,
                "field betas is not valid for type responses-api",
            ),
            (
                r#""type":"responses-api","auth":"bearer""#,
                "field auth is not valid for type responses-api",
            ),
            (
                r#""type":"responses-api","cache_system":true"#,
                "field cache_system is not valid for type responses-api",
            ),
            (
                r#""type":"responses-api","output_mode":"prompt""#,
                "output_mode prompt",
            ),
            (
                r#""type":"messages-api","store":false"#,
                "field store is not valid for type messages-api",
            ),
            (
                r#""type":"messages-api","output_mode":"json_object""#,
                "output_mode must be json_schema or prompt",
            ),
            (
                r#""type":"messages-api","responses_url":"https://x.test""#,
                "unknown field `responses_url`",
            ),
            (r#""type":"gemini-api""#, "invalid PROVIDERS_JSON"),
        ];
        for (fields, expected) in cases {
            let providers = format!(
                r#"{{"p":{{"url":"https://x.test/v1","api_key_env":"OPENAI_API_KEY",{fields}}}}}"#
            );
            let message = err_message(with_providers(&providers, "{}"));
            assert!(message.contains(expected), "{fields}: {message}");
        }
        let message = err_message(with_providers(
            r#"{"p":{"url":"https://x.test/v1","api_key_env":"OPENAI_API_KEY"}}"#,
            "{}",
        ));
        assert!(message.contains("missing field `type`"), "{message}");
    }

    #[test]
    fn rejects_reserved_headers_per_type() {
        for header in [
            "x-api-key",
            "anthropic-version",
            "anthropic-beta",
            "Authorization",
        ] {
            let providers = format!(
                r#"{{"p":{{"type":"messages-api","url":"https://x.test/v1","api_key_env":"OPENAI_API_KEY","headers":{{"{header}":"v"}}}}}}"#
            );
            assert!(err_message(with_providers(&providers, "{}")).contains("is reserved"));
        }
        // Responses providers keep the original reserved set.
        let c = with_providers(
            r#"{"p":{"type":"responses-api","url":"https://x.test/v1","api_key_env":"OPENAI_API_KEY","headers":{"anthropic-beta":"v"}}}"#,
            "{}",
        );
        assert!(c.is_ok());
    }

    #[test]
    fn rejects_blank_or_comma_betas() {
        for betas in [r#"[""]"#, r#"["a,b"]"#] {
            let providers = format!(
                r#"{{"p":{{"type":"messages-api","url":"https://x.test/v1","api_key_env":"OPENAI_API_KEY","betas":{betas}}}}}"#
            );
            assert!(err_message(with_providers(&providers, "{}")).contains("betas"));
        }
    }

    #[test]
    fn validates_route_rules() {
        let rejected = [
            (
                r#"{"provider":"openai","model":"m","thinking":"adaptive"}"#,
                "thinking applies only to messages-api",
            ),
            (
                r#"{"provider":"openai","model":"m","effort":"low","thinking":"disabled"}"#,
                "thinking applies only to messages-api",
            ),
            (
                r#"{"provider":"openai","model":"m","max_tokens":0}"#,
                "max_tokens must be 1..=128000",
            ),
            (
                r#"{"provider":"openai","model":"m","max_tokens":128001}"#,
                "max_tokens must be 1..=128000",
            ),
            (
                r#"{"provider":"openai","model":"m","effort":"extreme"}"#,
                "invalid MODEL_ROUTES_JSON",
            ),
            (
                r#"{"provider":"anthropic","model":"m","effort":"none"}"#,
                "effort none applies only to responses-api",
            ),
            (
                r#"{"provider":"anthropic","model":"m","effort":"minimal"}"#,
                "effort minimal applies only to responses-api",
            ),
            (
                r#"{"provider":"anthropic","model":"m","thinking":"disabled","effort":"xhigh"}"#,
                "thinking disabled",
            ),
            (
                r#"{"provider":"anthropic","model":"m","thinking":"disabled","effort":"max"}"#,
                "thinking disabled",
            ),
            (
                r#"{"provider":"anthropic","model":"m","max_tokens":0}"#,
                "max_tokens must be 1..=128000",
            ),
            (
                r#"{"provider":"anthropic","model":"m","max_tokens":128001}"#,
                "max_tokens must be 1..=128000",
            ),
            (
                r#"{"provider":"anthropic","model":"m","effort":"extreme"}"#,
                "invalid MODEL_ROUTES_JSON",
            ),
            (
                r#"{"provider":"anthropic","model":"m","thinking":"enabled"}"#,
                "invalid MODEL_ROUTES_JSON",
            ),
            (
                r#"{"provider":"missing","model":"m"}"#,
                "provider 'missing' is not configured",
            ),
        ];
        // DEFAULT_PROVIDER isolates these cases from default-provider selection.
        let with_routes = |routes: &str| {
            let mut vars = KEYS.to_vec();
            vars.extend([
                ("PROVIDERS_JSON", PROVIDERS),
                ("MODEL_ROUTES_JSON", routes),
                ("DEFAULT_PROVIDER", "anthropic"),
            ]);
            config(&vars)
        };
        for (route, expected) in rejected {
            let message = err_message(with_routes(&format!(r#"{{"r":{route}}}"#)));
            assert!(message.contains(expected), "{route}: {message}");
        }

        let accepted = with_routes(
            r#"{"a":{"provider":"anthropic","model":"m","thinking":"disabled","effort":"high","max_tokens":128000},
                "b":{"provider":"anthropic","model":"m","effort":"max","max_tokens":1}}"#,
        )
        .unwrap();
        assert_eq!(
            accepted.routes["a"].messages,
            Some(MessagesRoute {
                thinking: Thinking::Disabled,
                effort: Effort::High,
                max_tokens: 128_000,
            })
        );
        assert_eq!(accepted.routes["b"].messages.unwrap().effort, Effort::Max);
        assert!(accepted.routes["a"].responses.is_none());

        let accepted = with_routes(
            r#"{"plain":{"provider":"openai","model":"m"},
                "both":{"provider":"openai","model":"m","effort":"none","max_tokens":128000},
                "cap":{"provider":"openai","model":"m","max_tokens":1}}"#,
        )
        .unwrap();
        assert!(accepted.routes["plain"].responses.is_none());
        assert!(accepted.routes["plain"].messages.is_none());
        assert_eq!(
            accepted.routes["both"].responses,
            Some(ResponsesRoute {
                effort: Some(ResponsesEffort::None),
                max_output_tokens: Some(128_000),
            })
        );
        assert!(accepted.routes["both"].messages.is_none());
        assert_eq!(
            accepted.routes["cap"].responses,
            Some(ResponsesRoute {
                effort: None,
                max_output_tokens: Some(1),
            })
        );
        for effort in ["none", "minimal", "low", "medium", "high", "xhigh", "max"] {
            let c = with_routes(&format!(
                r#"{{"r":{{"provider":"openai","model":"m","effort":"{effort}"}}}}"#
            ))
            .unwrap();
            let r = c.routes["r"].responses.unwrap();
            assert_eq!(r.effort.unwrap().as_str(), effort);
            assert_eq!(r.max_output_tokens, None);
        }
    }

    #[test]
    fn body_limit_and_upstream_header_flag_defaults() {
        let c = config(&[("ANTHROPIC_API_KEY", "sk-a")]).unwrap();
        assert_eq!(c.max_body_bytes, 524_288);
        assert!(!c.expose_upstream_headers);
        let c = config(&[
            ("ANTHROPIC_API_KEY", "sk-a"),
            ("MAX_BODY_BYTES", "1024"),
            ("EXPOSE_UPSTREAM_HEADERS", "true"),
        ])
        .unwrap();
        assert_eq!(c.max_body_bytes, 1024);
        assert!(c.expose_upstream_headers);
    }

    #[test]
    fn request_read_timeout_default_override_and_minimum() {
        let c = config(&[("ANTHROPIC_API_KEY", "sk-a")]).unwrap();
        assert_eq!(c.request_read_timeout, Duration::from_millis(10_000));
        let c = config(&[
            ("ANTHROPIC_API_KEY", "sk-a"),
            ("REQUEST_READ_TIMEOUT_MS", "100"),
        ])
        .unwrap();
        assert_eq!(c.request_read_timeout, Duration::from_millis(100));
        let message = err_message(config(&[
            ("ANTHROPIC_API_KEY", "sk-a"),
            ("REQUEST_READ_TIMEOUT_MS", "99"),
        ]));
        assert!(message.contains("REQUEST_READ_TIMEOUT_MS"), "{message}");
    }

    #[test]
    fn model_catalog_defaults_and_overrides() {
        assert!(is_iso_date(DEFAULT_RELEASE_DATE));
        // The generated default route gets the default catalog entry.
        let c = config(&[("ANTHROPIC_API_KEY", "sk-a")]).unwrap();
        assert_eq!(
            c.models,
            BTreeMap::from([(
                "jev-latest".to_owned(),
                ModelInfo {
                    description: String::new(),
                    release_date: DEFAULT_RELEASE_DATE.to_owned(),
                },
            )])
        );

        let c = with_providers(
            PROVIDERS,
            r#"{"jev-latest":{"provider":"openai","model":"gpt-6-astra","description":"Fast","release_date":"2024-02-29"},
                "jev-claude":{"provider":"anthropic","model":"claude-haiku-5-5"}}"#,
        )
        .unwrap();
        let aliases: Vec<&str> = c.models.keys().map(String::as_str).collect();
        assert_eq!(aliases, ["jev-claude", "jev-latest"]);
        assert_eq!(c.models["jev-latest"].description, "Fast");
        assert_eq!(c.models["jev-latest"].release_date, "2024-02-29");
        assert_eq!(c.models["jev-claude"].description, "");
        assert_eq!(c.models["jev-claude"].release_date, DEFAULT_RELEASE_DATE);
        // Only route aliases are listed.
        assert_eq!(c.models.len(), c.routes.len());
    }

    #[test]
    fn rejects_malformed_release_date() {
        for date in [
            "2026-13-01",
            "2026-1-1",
            "2026-02-30",
            "2025-02-29",
            "1900-02-29",
            "2026-04-31",
            "2026-00-10",
            "2026-01-00",
            "+026-01-01",
            "2026-01-01T00:00:00Z",
            "",
        ] {
            let routes = format!(
                r#"{{"jev-latest":{{"provider":"openai","model":"m","release_date":"{date}"}}}}"#
            );
            let message = err_message(with_providers(PROVIDERS, &routes));
            assert!(
                message.contains("route jev-latest: release_date must be"),
                "{date}: {message}"
            );
        }
        for date in ["2024-02-29", "2000-02-29", "2026-12-31", "0001-01-01"] {
            assert!(is_iso_date(date), "{date}");
        }
    }

    #[test]
    fn passthrough_uses_default_provider_type_defaults() {
        let mut vars = KEYS.to_vec();
        vars.extend([
            ("PROVIDERS_JSON", PROVIDERS),
            ("MODEL_ROUTES_JSON", "{}"),
            ("ALLOW_PASSTHROUGH_MODEL", "true"),
        ]);
        let mut anthropic = vars.clone();
        anthropic.push(("DEFAULT_PROVIDER", "anthropic"));
        let c = config(&anthropic).unwrap();
        let (_, route, _) = c.resolve(Some("claude-sonnet-5-5")).unwrap();
        assert_eq!(route.model, "claude-sonnet-5-5");
        assert_eq!(route.messages, Some(MessagesRoute::default()));

        vars.push(("DEFAULT_PROVIDER", "openai"));
        let c = config(&vars).unwrap();
        let (_, route, provider) = c.resolve(Some("gpt-6-astra")).unwrap();
        assert_eq!(provider.name, "openai");
        assert!(route.messages.is_none());
        assert!(route.responses.is_none());
    }
}
