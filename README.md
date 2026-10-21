# jev-provider

[![CI](https://github.com/geekhoo/jev-provider/actions/workflows/ci.yml/badge.svg)](https://github.com/geekhoo/jev-provider/actions/workflows/ci.yml)

jev-provider is a small Rust gateway that serves the System One (Jev) decision API ([TypeSafe API reference](https://docs.typesafe.ai/api)) at `POST /v1/systemone`. It translates each bounded decision request for an upstream LLM that speaks the Anthropic Messages API or the OpenAI Responses API (the official services or compatible endpoints), then maps the model's structured output back into Jev-shaped `noul`, `choice`, and `score` answers.

**Status:** pre-release [v0.9.0](https://github.com/geekhoo/jev-provider/releases/tag/v0.9.0). CI runs on each published release, not on every push.

Each provider declares a `type` that selects its wire mapping:

| `type` | Upstream API | Status | `X-System-One-Response-Mode` |
|---|---|---|---|
| `responses-api` | OpenAI Responses API (and compatible endpoints) | implemented | `responses-adapted` |
| `messages-api` | Anthropic Messages API (and compatible endpoints) | implemented | `messages-adapted` |

The public Jev `model` is an alias. Its route picks a provider, and the provider's `type` decides how the request is built and the response parsed. The default route uses whichever provider is configured: `ANTHROPIC_API_KEY` alone gives the Anthropic Messages API with `claude-haiku-5-5` and adaptive thinking, and `OPENAI_API_KEY` alone gives the OpenAI Responses API with `gpt-6-astra`. When both are set, `DEFAULT_PROVIDER` must name the default (see [Choosing the default provider and model](#choosing-the-default-provider-and-model)). How the gateway compares with the official API is described in [`docs/compatibility.md`](docs/compatibility.md).

No provider SDK is used; the gateway calls the configured HTTP endpoint directly.

## Quick start

**Local, against the bundled mock upstream** (no keys, no network calls):

```bash
cargo run -p jev-mock                    # shell 1: mock upstream on 127.0.0.1:8090
# shell 2
export MOCK_API_KEY=mock-key BIND_ADDR=127.0.0.1:8080
export PROVIDERS_JSON='{"mock":{"type":"messages-api","url":"http://127.0.0.1:8090/v1/messages","api_key_env":"MOCK_API_KEY"}}'
cargo run -p jev-provider
curl -s localhost:8080/v1/systemone -H 'content-type: application/json' --data-binary @examples/request.json
```

[Local testing with jev-mock](#local-testing-with-jev-mock) covers more routes and the failure scenarios. To use a real provider instead, set `ANTHROPIC_API_KEY` or `OPENAI_API_KEY` (see [Build and run](#build-and-run)).

**Docker Compose:** copy `deploy/.env.ubuntu2604.example` to `deploy/.env.ubuntu2604`, then run `docker compose --profile mock up -d --build` from `deploy/`. See [Deploying with Docker Compose](#deploying-with-docker-compose).

## API

### Endpoints

- `POST /v1/systemone`: the System One decision request (`state`, typed `questions`, `model`). Gateway extensions: `model` is optional (defaults to `DEFAULT_JEV_MODEL`) and `user_instructions` is accepted. See [`examples/request.json`](examples/request.json).
- `GET /v1/models`: `{"models":[{"name","description","release_date"}]}` for the configured public aliases, sorted by name. `description` and `release_date` (`YYYY-MM-DD`) come from the route (defaults `""` and the gateway release date). Requires a gateway Bearer token when `GATEWAY_API_KEYS` is set. Upstream provider names and model IDs are never listed.
- `GET /api/system-one/config`: gateway extension reporting the default model and gateway limits. No token is needed, even when `GATEWAY_API_KEYS` is set.
- `GET /healthz`: gateway extension reporting process health. No token is needed.
- `GET /openapi.json`: gateway extension serving an OpenAPI 3.1.0 description of these routes, the request and response contracts, error codes, and headers, with `info.version` set to the gateway version. No token is needed, even when `GATEWAY_API_KEYS` is set: the document is static and holds no secrets or configured values (its limits are the defaults; `/api/system-one/config` reports the values in effect). The source is [`crates/jev-provider/src/openapi.json`](crates/jev-provider/src/openapi.json).

The official API has none of `/api/system-one/config`, `/healthz`, or `/openapi.json`. See [Gateway extensions](#gateway-extensions) and [Errors](#errors).

Example response shape (`model` is the route's `response_model`, see [Model routes](#model-routes-model_routes_json)):

```json
{
  "model": "jev-claude-2026-10-13",
  "answers": {
    "team": { "type": "choice", "choice": "billing", "confidence": 0.6, "probabilities": { "billing": 0.8, "support": 0.2 } },
    "urgency": { "type": "score", "score": 1.5, "confidence": 0.25,
                 "legend": { "0": "Normal review", "1": "Timely response", "2": "Immediate human attention" },
                 "probabilities": { "0": 0.1, "1": 0.3, "2": 0.6 } },
    "duplicate": { "type": "noul", "noul": 0.8 }
  },
  "usage": { "input_tokens": 314, "output_tokens": 91 }
}
```

`usage` is always present; a count the upstream did not report is `null`. Key order follows the official API: `answers` in the order the questions appear in the request, Choice `probabilities` in the order of the request's `criteria` keys, Score `probabilities` and `legend` by numeric level (`"10"` follows `"9"`). Inside an answer the fields come in the official order: Noul `type`, `noul`; Choice `type`, `choice`, `confidence`, `probabilities`; Score `type`, `score`, `confidence`, `legend`, `probabilities`. Numbers are rounded to 2 decimal places (see [Translation](#translation)).

### Errors

Every response carries the same `gw_<16 hex>` request ID in `x-request-id` and `x-typesafe-request-id`. Error bodies follow the official `detail` shape and add the gateway `error` member:

```json
{
  "detail": { "error_type": "authentication_error", "message": "Must supply an API key! Check your request and try again." },
  "error": { "code": "invalid_api_key", "message": "Must supply an API key! Check your request and try again.", "request_id": "gw_0000000000000001" },
  "request_id": "gw_0000000000000001"
}
```

- Authentication (when `GATEWAY_API_KEYS` is set), matching the official API (confirmed by probe on 2026-10-20):
  - A missing `Authorization` header, a scheme other than `Bearer` (e.g. `Basic x`), or `Bearer` with an empty token returns `403` with message "Must supply an API key! Check your request and try again."
  - A well-formed Bearer token that is not configured returns `401` with message "Cannot authenticate with the server. Please check your API key and try again."
  - Both use `error_type` `authentication_error` and `error.code` `invalid_api_key`.
  - The scheme name is case-insensitive (RFC 7235): `bearer <key>` and `BEARER <key>` work like `Bearer <key>`.
- Unknown paths return `404` with `"detail": "Not Found"`; any method other than `POST` on `/v1/systemone` returns `405` with `"detail": "Method Not Allowed"` and `Allow: POST`.
- Other errors (400, 408, 409, 413, 415, 422, 5xx) use `detail.error_type` = the gateway `error.code`. The gateway's own include `408 request_timeout` (the body did not arrive within `REQUEST_READ_TIMEOUT_MS`), `413 request_too_large`, `503 provider_busy` (no provider permit within `ADMISSION_WAIT_MS`), and `504 provider_timeout` (an upstream attempt exceeded the provider `timeout_ms`).
- Upstream non-2xx: 429 and 529 keep their status (`provider_busy`, `retry-after` forwarded) and 5xx keep theirs (`provider_unavailable`). Every other upstream status, including 400, 401, 403, 404, and 422, becomes `502 provider_rejected_request` with the upstream status only in the message, so a caller never sees an upstream 4xx as its own. Upstream error bodies are never echoed.
- From an upstream 200: a refusal returns `422 provider_refused` (with the Messages refusal category when present), and output cut off by the token limit returns `502 provider_output_truncated`. Unusable output (bad JSON, no text, oversize) returns `502 invalid_provider_response`.

### Gateway limits

These are gateway policy, not official System One limits: request body 524,288 bytes (`MAX_BODY_BYTES`); 32 questions (`MAX_QUESTIONS`); 255 Choice options (`MAX_CHOICE_OPTIONS`); 1..=10 Score levels (`MAX_SCORE_LEVELS`); JSON depth 32 (`MAX_JSON_DEPTH`); question and option names up to 128 characters (a constant). The byte, question-count, depth, and name limits have no documented official counterpart.

The official limits, per the [TypeSafe models docs](https://docs.typesafe.ai/models), are 64k tokens per request, 32k tokens for `state` plus the longest question, 255 Choice options, and 2..=10 Score levels. The official API also accepts a Score with 1 level (probe of 2026-10-20), and so does the gateway: the answer is `score` 0, probability 1 and `confidence` 1. The gateway does not count tokens; the upstream provider's own limits apply. System One billing, credits, and hosted rate limits are not implemented.

### Question and state rules

These rules are gateway policy. They are stricter than the official API. The only official-side rules seen so far come from LiteLLM's request model, not TypeSafe: it rejects a Noul with neither `instructions` nor `criteria`, and a `state` that is `null`, a number or a boolean (see [Rejected by the proxy only](docs/compatibility.md#rejected-by-the-proxy-only)). A request that breaks one gets `422 invalid_request` with a message that names the question (or `state`) and the rule.

Text is counted in units over every object key and string value, at any depth:

- Each Han, Hiragana, Katakana or Thai character is one unit (these scripts do not put spaces between words). `"生产环境"` is 4 units.
- Other text counts runs of word characters (letters, digits, `_`), like the regex `\w+`. `"Is the service down?"` is 4 units; Korean counts by words.
- Numbers, booleans and `null` count nothing. `""`, whitespace, `{}` and `[]` are 0 units.

Text is meaningful when it has at least `MIN_DESCRIPTION_WORDS` units (default 3, minimum 1).

| Type | Passes when |
| --- | --- |
| `noul` | `instructions` is meaningful, or `criteria` is a positive/negative pair whose two descriptions are both meaningful |
| `choice` | `instructions` is meaningful, or every option's description is meaningful |
| `score` | `instructions` is meaningful, or every level's description is meaningful |

With meaningful `instructions`, Choice and Score descriptions are optional (`""` or `null` is fine), and Noul `criteria` may be absent, a string (passed to the model as context), or a pair with any descriptions.

A Noul pair is an object with exactly two keys, one positive and one negative key from the same row. Keys match case-insensitively (`Agree`/`DISAGREE` is fine); keys from two rows (`agree`/`no`) or a third key are rejected, as are arrays, numbers and booleans. Without meaningful `instructions` a string `criteria` is rejected: write the question in `instructions` or use a pair.

| Positive | Negative |
| --- | --- |
| `true` | `false` |
| `yes` | `no` |
| `agree` | `disagree` |
| `accept` | `reject` |
| `approve` | `deny` |
| `allow` | `block` |
| `pass` | `fail` |
| `positive` | `negative` |

The model sees a pair as `{"true": <positive description>, "false": <negative description>}` and returns P(true). The official API accepts only `true`/`false` keys; the other rows are a gateway extension.

`state` is rejected when it is `null`, a number or a boolean; when it is an array holding only numbers, booleans or `null` at any depth (`[1, true, [2]]`); or when it has fewer than `MIN_DESCRIPTION_WORDS` units. So `{"temp": 38.5, "fever": true}` (2 units) is rejected and `{"temp": 38.5, "fever": true, "cough": "dry"}` passes.

### Gateway extensions

None of these has an official System One counterpart, or the official behavior is unknown. Official clients can ignore them:

- `user_instructions` (closed and typed; see below) and an optional request `model`.
- `Idempotency-Key` request header (see [Throughput and latency design](#throughput-and-latency-design)).
- `GET /api/system-one/config`, `GET /healthz`, and `GET /openapi.json`.
- `X-System-One-Response-Mode`, `X-System-One-Error-Source`, and `X-System-One-Error-Code` response headers. `X-Upstream-Provider`, `X-Upstream-Model`, and `X-Upstream-Request-Id` name upstream providers and models, so they are off by default; `EXPOSE_UPSTREAM_HEADERS=true` enables them.
- The `error` member (`code`, `message`, `request_id`) and top-level `request_id` in error bodies, `X-Idempotency-Replayed` on replays, and `Cache-Control: no-store` on every response.
- Strictness: the [question and state rules](#question-and-state-rules); `415` for a non-JSON `Content-Type`; `400` for duplicate JSON object keys and for malformed JSON; unknown fields inside `user_instructions` rejected with `422`.
- Gateway-generated errors from an upstream 200: `422 provider_refused` and `502 provider_output_truncated`.

### Typed `user_instructions`

An optional extension for bounded domain context. Its fields are fixed (`deny_unknown_fields`): `domain`, `locale`, `objective`, `risk_posture`, `uncertainty_policy`, `terminology`, `constraints`, `reference_facts`, `decision_rules`, and `tags`, each with explicit cardinality and length limits. It is not a free-form prompt field. The full contract is [`schema/user-instructions.schema.json`](schema/user-instructions.schema.json).

### Translation

- `state` is framed as untrusted evidence; the system instructions forbid following instructions inside it.
- Question names become sorted opaque aliases (`q0`, `q1`, ...), and options and levels get aliases too (`o0`, `l0`), so equivalent topologies share a schema shape. Caller names, and the request order, are restored after inference.
- Option labels carry meaning, so the model sees them: Choice options are listed as `[{"o0": {"<label>": "<description>"}}, ...]` in alias order, Score levels as `[{"l0": "<description>"}, ...]` lowest first, with `""` for a `null` description. Question names stay hidden. A Noul pair is shown as `{"true": ..., "false": ...}`.
- The model returns only probabilities. The gateway derives Choice `choice` (arg-max), Score `score` (`sum(level * p)`) and `legend` (caller criteria), and `confidence` with the [official formulas](https://docs.typesafe.ai/confidence). Distributions are normalized; a zero or invalid distribution falls back to uniform. Every number (`noul`, `score`, probabilities, `confidence`) is rounded to 2 decimal places, as the official API does. `choice`, `score`, and `confidence` come from the normalized values before rounding, so the rounded probabilities need not sum to exactly 1.
  - Choice: `(p_max − 1/n) / (1 − 1/n)`, and 1.0 when `n ≤ 1`. Probabilities (0.8, 0.2) give 0.6.
  - Score: `max(0, 1 − Σ pᵢ·|i − m| / MAD_unif)`, where `m` is the most likely level (the first one on a tie) and `MAD_unif = (1/n)·Σ |i − (n−1)/2|`, and 1.0 when `n = 1`. Probabilities (0.1, 0.3, 0.6) give 0.25.

**Probability caveat:** you get wire and semantic compatibility, not Jev's proprietary calibration. Recalibrate thresholds against labeled examples before anything acts on the answers automatically.

## Configuration

### Choosing the default provider and model

Without `PROVIDERS_JSON`, the built-in registry holds one provider per key that is set (an empty value counts as unset):

| Key set | Provider | `type` | URL | Model variable (default) |
|---|---|---|---|---|
| `ANTHROPIC_API_KEY` | `anthropic` | `messages-api` | `https://api.anthropic.com/v1/messages` | `ANTHROPIC_MODEL` (`claude-haiku-5-5`), adaptive thinking, effort `medium`, `max_tokens` 16000 |
| `OPENAI_API_KEY` | `openai` | `responses-api` | `https://api.openai.com/v1/responses` | `OPENAI_MODEL` (`gpt-6-astra`), `store: false`, `output_mode: json_schema` |

Startup then picks the default provider, used by the default alias `DEFAULT_JEV_MODEL` (`jev-latest`) and by passthrough:

| Configured providers | Default |
|---|---|
| exactly one (built-in or `PROVIDERS_JSON`) | that provider; `DEFAULT_PROVIDER` is optional but must match it |
| more than one | `DEFAULT_PROVIDER`, required; without it, a `MODEL_ROUTES_JSON` route for the default alias selects its provider; otherwise startup fails and lists the configured providers |
| none | startup fails, naming `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, and `PROVIDERS_JSON` |

A `DEFAULT_PROVIDER` that names an unconfigured provider is a startup error. When `MODEL_ROUTES_JSON` is unset, the default route uses the default provider's model variable by type: `ANTHROPIC_MODEL` for `messages-api`, `OPENAI_MODEL` for `responses-api`. Startup errors name environment variables, never key values.

```bash
# Both keys: choose the default explicitly.
export ANTHROPIC_API_KEY='sk-ant-...'
export OPENAI_API_KEY='sk-...'
export DEFAULT_PROVIDER=openai
export OPENAI_MODEL=gpt-6-astra
```

[`.env.example`](.env.example) shows each setup: Anthropic only, OpenAI only, both keys with either default, an explicit registry with routes, and the local `jev-mock` upstream (block E).

### Provider registry (`PROVIDERS_JSON`)

When set, `PROVIDERS_JSON` replaces the built-in registry above; every listed provider's `api_key_env` must be set.

Common fields:

| Field | Default | Notes |
|---|---|---|
| `type` | required | `responses-api` or `messages-api` |
| `url` | required | `http` or `https` endpoint |
| `api_key_env` | required | name of the environment variable holding the key; the value is never placed in JSON or logged |
| `headers` | `{}` | custom headers; `Authorization`, `Content-Type`, `Content-Length`, `Host` are reserved |
| `timeout_ms` | 20000 (`responses-api`), 60000 (`messages-api`) | total deadline per attempt (not per request); values below 100 are raised to 100 |
| `max_retries` | 0 | capped at 3; see [Retries and deadlines](#retries-and-deadlines) |
| `max_in_flight` | `MAX_IN_FLIGHT` | per-provider concurrency permits |

`responses-api` fields: `store` (default `false`; `null` omits it), `output_mode` `json_schema` (default, strict Structured Outputs) | `json_object` | `text` (the non-schema modes append the schema to the input). Auth is `Authorization: Bearer <key>`.

`messages-api` fields: `anthropic_version` (default `2023-06-01`), `betas` (joined into `anthropic-beta`, omitted when empty), `auth` `x_api_key` (default) | `bearer`, `cache_system` (default `true`), `output_mode` `json_schema` (default) | `prompt`. `x-api-key`, `anthropic-version`, and `anthropic-beta` are also reserved custom header names.

A field that belongs to the other type, an `output_mode` from the other type, or any unknown field is a startup configuration error.

**Migrating from `jev2openaires`:** rename `responses_url` to `url` and add `"type": "responses-api"` to each provider. The old field and a missing `type` fail at startup; `OPENAI_RESPONSES_URL` is not read.

#### Retries and deadlines

Retried statuses differ by type: `responses-api` retries 429, 502, 503, 504, and 529; `messages-api` also retries 500. Refused (`provider_refused`), truncated (`provider_output_truncated`), and rejected (`provider_rejected_request`) requests are never retried by the gateway, because they would fail the same way again. Local `invalid_provider_response` failures (bad JSON, no text, oversize, and valid JSON of the wrong shape, such as output without `answers`) are retried. Backoff is `20 ms × 2^attempt`.

A route's worst-case latency is `(max_retries + 1) × timeout_ms` plus backoff. Keep `max_retries` at 0 (the default) or 1; 1 is reasonable for `messages-api` routes. The official SDKs retry the gateway's 5xx answers themselves (see [Using the official SDKs](#using-the-official-sdks)).

#### Messages API mapping

Each request sends the system instructions as `system` (a cached text block with `cache_control: ephemeral`, or a plain string with `cache_system: false`), the prepared input as one user message, `max_tokens`, `thinking`, and `output_config.effort` from the route. In `json_schema` mode, `output_config.format` carries the generated schema with numeric and length bounds removed, since the Messages API rejects them; probabilities are still clamped and normalized by the gateway. In `prompt` mode the schema is appended to the user message instead; `thinking` and `effort` are still sent, and Messages-compatible endpoints without structured outputs may reject them. `temperature`, `top_p`, `top_k`, `stop_sequences`, `stream`, tools, `budget_tokens`, and assistant prefill are never sent.

The response's `stop_reason` is checked first: `end_turn` continues, `refusal` returns `422 provider_refused`, `max_tokens` or `model_context_window_exceeded` returns `502 provider_output_truncated`, and anything else returns `502 invalid_provider_response`. The first `text` content block (after any thinking blocks) is parsed as the answer. `usage.input_tokens` adds cache-read and cache-creation tokens; `output_tokens` includes thinking.

Thinking rules: `claude-haiku-5-5` uses adaptive thinking (`budget_tokens` is rejected); `thinking: disabled` is only valid up to effort `high`, so the gateway rejects it with `xhigh` or `max` at startup. Thinking tokens count toward `max_tokens`.

Large topologies: the output budget has to cover thinking plus the JSON answer. The default `max_tokens` 16000 fits typical requests. A request near the gateway limits (32 questions × 255 Choice options) needs roughly 60–80K output tokens, so raise `max_tokens` (up to 128000) or lower `effort` on such routes. Very large topologies can also exceed a provider's schema-size limits (about 8,225 schema properties at 32 × 255); keep `MAX_QUESTIONS × MAX_CHOICE_OPTIONS` within what your provider accepts.

### Model routes (`MODEL_ROUTES_JSON`)

```json
{
  "jev-latest": { "provider": "anthropic", "model": "claude-haiku-5-5", "response_model": "jev-claude-2026-10-13",
                  "description": "Default decision model", "release_date": "2026-10-13" },
  "jev-deep":   { "provider": "anthropic", "model": "claude-sonnet-5-5", "response_model": "jev-deep-2026-10-13",
                  "effort": "high", "max_tokens": 32000 },
  "jev-openai": { "provider": "openai", "model": "gpt-6-astra", "response_model": "jev-openai-2026-10-13",
                  "effort": "medium", "max_tokens": 32000 }
}
```

Fields: `provider`, `model`, optional `response_model` (the `model` reported to callers), `description` (default `""`) and `release_date` (`YYYY-MM-DD`, default the gateway release date; malformed is a startup error) for `/v1/models`, and generation settings. `effort` and `max_tokens` work on both provider types, with different defaults and wire fields:

- `messages-api` routes: `thinking` (`adaptive` default | `disabled`), `effort` (`low` | `medium` default | `high` | `xhigh` | `max`), and `max_tokens` (default 16000, `1..=128000`). `thinking: disabled` cannot be combined with effort `xhigh` or `max`, and effort `none` or `minimal` is rejected.
- `responses-api` routes: `effort` (`none` | `minimal` | `low` | `medium` | `high` | `xhigh` | `max`), sent as `reasoning.effort`, and `max_tokens` (`1..=128000`), sent as `max_output_tokens` (reasoning plus visible output). Both have no default: when unset they are omitted, so the model's own defaults apply. Supported effort values vary by model; an unsupported one is rejected upstream with HTTP 400. Output cut off at the cap returns `502 provider_output_truncated`.

`thinking` on a `responses-api` route, an out-of-range value, or naming an unconfigured provider is a startup error.

The official API reports a versioned model ID in the response `model`, even when the request used an alias. Set a versioned `response_model` on each route (for example `jev-claude-2026-10-13`) and change it when the route's upstream model or settings change. Never derive it from upstream model IDs, and never report an official Jev version (such as `jev-1.13.0`) for a different model. Without `response_model`, the response `model` is the requested alias.

When unset, one route maps `DEFAULT_JEV_MODEL` to the default provider with that provider type's model variable (see [Choosing the default provider and model](#choosing-the-default-provider-and-model)). With `ALLOW_PASSTHROUGH_MODEL=true`, unknown aliases are sent as upstream model IDs on the default provider with that provider type's defaults.

Example with both types, each route with a versioned `response_model`:

```bash
export ANTHROPIC_API_KEY=...   # values live in the environment, never in JSON
export OPENAI_API_KEY=...
export DEFAULT_PROVIDER=anthropic
export PROVIDERS_JSON='{
  "anthropic": { "type": "messages-api", "url": "https://api.anthropic.com/v1/messages",
                 "api_key_env": "ANTHROPIC_API_KEY", "timeout_ms": 60000, "max_retries": 1 },
  "openai":    { "type": "responses-api", "url": "https://api.openai.com/v1/responses",
                 "api_key_env": "OPENAI_API_KEY", "store": false, "timeout_ms": 20000 }
}'
export MODEL_ROUTES_JSON='{
  "jev-latest": { "provider": "anthropic", "model": "claude-haiku-5-5", "response_model": "jev-claude-2026-10-13" },
  "jev-openai": { "provider": "openai", "model": "gpt-6-astra", "response_model": "jev-openai-2026-10-13" }
}'
```

### Environment variables

At least one provider is required: `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, or `PROVIDERS_JSON`. Empty values count as unset.

| Variable | Default | Purpose |
|---|---:|---|
| `BIND_ADDR` | `0.0.0.0:8080` | listener |
| `GATEWAY_API_KEYS` | empty | comma-separated accepted Bearer tokens; empty disables local auth |
| `ANTHROPIC_API_KEY` | unset | adds the built-in `anthropic` (`messages-api`) provider |
| `ANTHROPIC_MODEL` | `claude-haiku-5-5` | default-route model when the default provider is `messages-api` |
| `OPENAI_API_KEY` | unset | adds the built-in `openai` (`responses-api`) provider |
| `OPENAI_MODEL` | `gpt-6-astra` | default-route model when the default provider is `responses-api` |
| `DEFAULT_JEV_MODEL` | `jev-latest` | public default alias |
| `DEFAULT_PROVIDER` | the only configured provider | provider for the default route and passthrough; required when more than one is configured |
| `ALLOW_PASSTHROUGH_MODEL` | `false` | permit unknown aliases as upstream IDs |
| `EXPOSE_UPSTREAM_HEADERS` | `false` | send `X-Upstream-Provider`, `X-Upstream-Model`, and `X-Upstream-Request-Id` |
| `PROVIDERS_JSON` | built-in registry from the keys above | provider registry |
| `MODEL_ROUTES_JSON` | default single route | alias routing |
| `MAX_IN_FLIGHT` | `1024` | default provider concurrency bound |
| `ADMISSION_WAIT_MS` | `5` | wait for a provider permit before load shedding |
| `POOL_MAX_IDLE_PER_HOST` | `256` | idle pooled connections per authority |
| `POOL_IDLE_TIMEOUT_MS` | `90000` | pooled idle lifetime |
| `MAX_BODY_BYTES` | `524288` | caller request bound |
| `REQUEST_READ_TIMEOUT_MS` | `10000` | bound on reading request headers and body (minimum 100; lower is a startup error) |
| `MAX_UPSTREAM_BYTES` | `1048576` | upstream success/error body bound |
| `MAX_QUESTIONS` | `32` | request bound |
| `MAX_CHOICE_OPTIONS` | `255` | Choice bound |
| `MAX_SCORE_LEVELS` | `10` | Score bound |
| `MAX_JSON_DEPTH` | `32` | state/question JSON nesting bound |
| `MIN_DESCRIPTION_WORDS` | `3` | text units that make instructions, descriptions or `state` meaningful (minimum 1; 0 or less is a startup error); see [Question and state rules](#question-and-state-rules) |
| `IDEMPOTENCY_TTL_SECONDS` | `86400` | cache retention |
| `IDEMPOTENCY_MAX_ENTRIES` | `100000` | approximate process-level cache cap |
| `SHUTDOWN_GRACE_MS` | `10000` | graceful drain bound |

### Using the official SDKs

The official TypeSafe SDKs ([Python](https://docs.typesafe.ai/sdk/python/api/retries), [JavaScript](https://docs.typesafe.ai/sdk/javascript/api)) use a 10 s per-attempt timeout and retry 408, 429, and 5xx twice. Against this gateway:

- A route's worst case is `(max_retries + 1) × timeout_ms` plus backoff. Set the SDK `timeout` above it, or lower `timeout_ms` for latency-sensitive routes. The default `messages-api` `timeout_ms` (60000) is well above the SDK's 10 s, so with SDK defaults a slow call is abandoned and retried while the first upstream call is still billed.
- The gateway never retries refused, truncated, or rejected requests, but the SDKs retry its 502 and 5xx answers, including `provider_output_truncated` and `provider_rejected_request`, so a deterministic failure can cost up to `3 × (max_retries + 1)` upstream calls. Keep `max_retries` at 0–1, and leave 429 retries to the SDK.
- With `Idempotency-Key`, an SDK retry that arrives while the first attempt is still running gets `409 request_in_progress`, which the SDKs do not retry. After any gateway error, the same key returns `409 idempotency_failed` for `IDEMPOTENCY_TTL_SECONDS`. Use a new key for each logical retry.

## Deployment

### Build and run

Rust 1.85 (pinned in `rust-toolchain.toml`). From the workspace root:

```bash
cargo build --release
export ANTHROPIC_API_KEY='sk-ant-...'
export GATEWAY_API_KEYS='local-dev-key'
./target/release/jev-provider
```

```bash
curl --fail-with-body http://127.0.0.1:8080/v1/systemone \
  -H 'Authorization: Bearer local-dev-key' \
  -H 'Content-Type: application/json' \
  -H 'Idempotency-Key: test-001' \
  --data-binary @examples/request.json
```

Docker:

```bash
docker build -t jev-provider .
docker run --rm -p 8080:8080 -e ANTHROPIC_API_KEY="$ANTHROPIC_API_KEY" -e GATEWAY_API_KEYS='local-dev-key' jev-provider
```

Inbound TLS is left to a reverse proxy or load balancer.

### Deploying with Docker Compose

[`deploy/compose.yaml`](deploy/compose.yaml) runs `jev-provider` (built from the repository `Dockerfile`, published on port 8080) by default. The bundled `jev-mock` (published on `127.0.0.1:8090` only, for `GET /_mock/requests`) is in the `mock` profile and starts only with `--profile mock`. Both use `restart: always`. The gateway reads its configuration only from `deploy/.env.ubuntu2604`, which is git-ignored. The tracked template, [`deploy/.env.ubuntu2604.example`](deploy/.env.ubuntu2604.example), starts on the mock upstream (block M) and documents the real-provider setups, so on a first run either start with `--profile mock` or switch to a real provider block.

```bash
cd deploy
cp .env.ubuntu2604.example .env.ubuntu2604     # git-ignored; set GATEWAY_API_KEYS
docker compose --profile mock up -d --build    # gateway + jev-mock (template block M)
docker compose up -d --build                   # gateway only (real provider block)
```

To stop and remove a `jev-mock` left over from an earlier run:

```bash
docker compose --profile mock stop jev-mock && docker compose --profile mock rm -f jev-mock
```

`docker compose restart` does not re-read `env_file`. After editing `.env.ubuntu2604`, recreate the gateway container:

```bash
docker compose up -d --force-recreate jev-provider
```

### Throughput and latency design

- Tokio multi-thread runtime with no application-level worker queue.
- One process-wide pooled Hyper client (HTTP/1.1 keep-alive and HTTP/2, rustls) shared by every provider (`POOL_MAX_IDLE_PER_HOST`, `POOL_IDLE_TIMEOUT_MS`).
- Per-provider semaphore admission: if a permit is not available within `ADMISSION_WAIT_MS`, the gateway returns `503 provider_busy` instead of queueing.
- Inbound and upstream bodies are read frame by frame against `MAX_BODY_BYTES` and `MAX_UPSTREAM_BYTES`.
- `REQUEST_READ_TIMEOUT_MS` bounds slow clients: an HTTP/1 connection that does not send complete request headers in time (including an idle keep-alive connection) is closed, and a body not received in time gets `408 request_timeout`. The pre-413 drain stops at the same deadline and the 413 is still returned.

  Notes for client authors:
  - Because the header timeout also closes keep-alive connections idle longer than `REQUEST_READ_TIMEOUT_MS` (10 s by default), a request sent on a reused idle connection can fail when the gateway closes it at the same moment. Retry that request on a new connection, as pooled HTTP clients normally do.
  - After a 408, or after a 413 whose drain stopped at `MAX_BODY_BYTES`, unread body bytes may still be in flight when the gateway closes the connection, and the TCP stack can answer them with a reset. A client that keeps sending after the error may see a connection reset instead of the response.
- `timeout_ms` bounds the whole attempt (connect, headers, body); the future is dropped on timeout and the attempt fails with `504 provider_timeout`.
- Retries default to 0, are capped at 3, and use short deterministic backoff.
- `Idempotency-Key` uses a 64-shard, process-local, caller-scoped cache: in-progress → `409 request_in_progress`, different body → `409 idempotency_conflict`, replay → cached response with `X-Idempotency-Replayed: true`, failed original → `409 idempotency_failed`. Multi-replica deployments need sticky routing or a shared store.
- `TCP_NODELAY` on accepted sockets; SIGTERM or Ctrl-C stops accepting and drains open connections within `SHUTDOWN_GRACE_MS`.
- Release profile: thin LTO, one codegen unit, `panic = "abort"`, stripped symbols.

### Security notes

- Provider keys come only from the named environment variables and are never returned or logged.
- `user_instructions` is a bounded semantic object, not a system-prompt override; system instructions stay separate.
- Generated output schemas use opaque IDs and `additionalProperties: false`.
- Upstream bodies are size-bounded before parsing; custom headers cannot override auth or framing headers.
- Responses `store` defaults to `false`; gateway responses use `Cache-Control: no-store`.

## Local testing with jev-mock

`crates/jev-mock` is a deterministic upstream that serves `POST /v1/responses` and `POST /v1/messages`, checks each request against the provider contract (a violation returns 400 in that provider's error shape), and answers from the request's own schema. It needs no keys and makes no network calls.

```bash
cargo run -p jev-mock                       # listens on 127.0.0.1:8090
# second shell
export MOCK_API_KEY=mock-key BIND_ADDR=127.0.0.1:8080 DEFAULT_PROVIDER=mock-anthropic
export PROVIDERS_JSON='{
  "mock-anthropic": {"type":"messages-api","url":"http://127.0.0.1:8090/v1/messages","api_key_env":"MOCK_API_KEY","timeout_ms":5000},
  "mock-openai":    {"type":"responses-api","url":"http://127.0.0.1:8090/v1/responses","api_key_env":"MOCK_API_KEY","timeout_ms":5000},
  "mock-refusal":   {"type":"messages-api","url":"http://127.0.0.1:8090/v1/messages","api_key_env":"MOCK_API_KEY","headers":{"x-mock-scenario":"refusal"}}
}'
export MODEL_ROUTES_JSON='{
  "jev-latest":  {"provider":"mock-anthropic","model":"claude-haiku-5-5"},
  "jev-openai":  {"provider":"mock-openai","model":"gpt-mock"},
  "jev-refusal": {"provider":"mock-refusal","model":"claude-haiku-5-5"}
}'
cargo run -p jev-provider
curl -s localhost:8080/v1/systemone -H 'content-type: application/json' --data-binary @examples/request.json
curl -s 'localhost:8090/_mock/requests?limit=1'
```

The scenario comes from the `x-mock-scenario` request header (set it per provider through `headers`), else `MOCK_SCENARIO`, else `ok`: `ok`, `refusal`, `max_tokens`, `no_text`, `bad_json`, `oversize[:<bytes>]`, `no_usage`, `http_<status>` (400–599; `http_429` adds `retry-after: 7`), `slow:<ms>` (capped at 120000), `flaky:<n>` (the first `n` requests return 503), and `wrong_shape[:<n>]` (a successful reply whose output is valid JSON without `answers`; with `:<n>`, only the first `n` requests, then `ok`). `GET /_mock/requests?limit=N` returns captured requests with secrets redacted and any contract errors; `DELETE /_mock/requests` clears them.

| Mock variable | Default | Purpose |
|---|---:|---|
| `MOCK_BIND` | `127.0.0.1:8090` | listener |
| `MOCK_SCENARIO` | `ok` | scenario when the request has no `x-mock-scenario` header |
| `MOCK_CAPTURE` | `100` | captured requests kept |
| `MOCK_MAX_BODY_BYTES` | `4194304` | request body bound |
| `MOCK_REQUIRE_KEY` | unset | exact upstream key to require; unset accepts any non-empty key |

`cargo test` runs the same setup in process: the end-to-end suites in `crates/jev-provider/tests/` start the mock and the gateway on ephemeral ports.

## Test corpus and results

[`examples/corpus/`](examples/corpus/README.md) holds 25 decision requests: 5 domains (e-commerce, IT operations, payments, moderation, insurance) × 5 complexity levels (L1–L5). Each has one Noul, one Choice and one Score question plus a `user_instructions` block, and one question in each case is built to depend on `user_instructions`.

- `run.sh` and `run.ps1` send the cases to one gateway route and save each response.
- `run_async.py` sends every case, with and without `user_instructions`, to several gateway routes concurrently.
- `run_official.py` sends them to an official-style `/v1/systemone` endpoint (for example LiteLLM).
- `analyse.py` compares every answer set present. [`ANALYSIS.md`](examples/corpus/ANALYSIS.md) records the results, and [`results.html`](examples/corpus/results.html) charts them on one self-contained page.

Headline findings from [`ANALYSIS.md`](examples/corpus/ANALYSIS.md) (ten answer sets, 500 answers, 2026-10-21):

- With `user_instructions`, two sets pick the same top Choice option in 21.1 of 25 cases on average; without them, 15.7.
- A model behind the gateway agrees with the same model answering directly in 23–24 of 25 cases, so the gateway passes each model's judgement through unchanged.
- L2 cases, which hinge on `risk_posture` alone, are the weak level: 76% of top choices match the consensus, against 90–98% at the other levels.
- The OpenAI models and the official Jev API report Choice confidence of 0.72–0.83; the other sets sit near 0.5.
- No set followed the injected instruction in the L5 cases.

## Workspace

| Path | Contents |
|---|---|
| `crates/jev-core` | Library crate `jev_core`, with no server or runtime code: Jev contract types, validation, strict JSON parsing, idempotency cache, prompt and schema preparation, answer mapping, and provider dispatch (`provider::{build_request, parse_response}` matched on `ApiType`). |
| `crates/jev-core/src/provider/responses.rs` | Responses API request body (optional `reasoning.effort` and `max_output_tokens` from the route) and output extraction. |
| `crates/jev-core/src/provider/messages.rs` | Messages API request body (schema without numeric bounds, thinking, effort) and response parsing (`stop_reason`, first `text` block, usage). |
| `crates/jev-provider` | Library and binary `jev-provider`: environment configuration and validation, Hyper server, auth, bounded reads, idempotency, and the shared upstream loop (pooled client, admission, deadline, retries). |
| `crates/jev-provider/tests` | End-to-end tests: the gateway and `jev-mock` in process on ephemeral ports. Unit tests stay inline in each module. |
| `crates/jev-mock` | Deterministic mock upstream for the Responses and Messages APIs, with contract checks, scenarios, and request capture (see [Local testing with jev-mock](#local-testing-with-jev-mock)). |
| `prompts/system-instructions.txt` | Exact runtime system instructions (kept in sync with `SYSTEM_INSTRUCTIONS` in `crates/jev-core/src/prompt.rs`). |
| `schema/user-instructions.schema.json` | Caller-facing `user_instructions` JSON Schema. |
| `examples/request.json` | Complete request example. |
| `examples/corpus/` | Decision-request test corpus, run scripts and analysis (see [Test corpus and results](#test-corpus-and-results)). |
| `Dockerfile` | Two-stage image (`rust:1.85-bookworm` build, `debian:bookworm-slim` runtime) holding only the `jev-provider` binary, run as uid 10001. |
| `deploy/` | Docker Compose deployment: `compose.yaml` (the gateway, plus the bundled `jev-mock` in the `mock` profile) and `.env.ubuntu2604.example` (see [Deploying with Docker Compose](#deploying-with-docker-compose)). |

Dependencies are infrastructure only: `tokio`, `hyper`, `hyper-util`, `http-body-util`, `hyper-rustls`, `serde`/`serde_json`, and `bytes`. No web framework or HTTP client wrapper sits on top.

## Verification status

The official contract behavior described in [`docs/compatibility.md`](docs/compatibility.md) (confidence formulas, `usage`, `/v1/models`, error bodies, auth statuses, request-ID headers, `Allow`, upstream status mapping, limits, and upstream headers) is implemented, with unit and end-to-end tests against `jev-mock`. Some defaults are inferred, not confirmed by official documentation: the Score tie rule, `null` usage counts, `/v1/models` defaults, non-auth `error_type` values, and the gateway-only limits.

Probes of the official API on 2026-10-20 confirmed the confidence formulas and found differences. The gateway now follows four of them: auth statuses and messages, 2-decimal rounding, request order in answers, and 1-level Score questions. For questions and `state` it applies its own, stricter [rules](#question-and-state-rules). It keeps its own request IDs, error members, and stricter limits by choice; see [probe results](docs/compatibility.md#probe-results).

The CI workflow ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)) runs on GitHub Actions for each published release and on manual dispatch, not on every push. It checks formatting, runs Clippy with warnings as errors, runs the tests, builds the release binary and builds the Docker image. The v0.9.0 code passed it. Live calls to the real provider APIs are tested by hand. The container image and the Compose stack were checked on Linux on 2026-10-20 (build, image contents, `/healthz`, the bundled-mock flow, and the `docker stop` drain check). In the drain check a request in flight during `docker stop` still returned 200, and `docker stop` returned in 2.14 s.

## What this gateway does not claim

It reproduces the Jev API contract and its bounded-decision semantics, not Jev's trained model. Calibration, latency, and confidence depend on the provider and model you configure. Run shadow traffic and pick thresholds from your own workload.
