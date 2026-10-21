# Compatibility with the official System One API

This page compares jev-provider with the official TypeSafe System One API (model family Jev). It covers what the gateway matches, where it differs on purpose, what live probes of the official API confirmed, and what is still unknown.

The official documentation was read on 2026-10-13. The official API was probed without a key on 2026-10-13 and with a key on 2026-10-20. Gateway behavior below describes the current code; the [README](../README.md) is the reference for its configuration.

## Sources

| Source | Kind | What it gives |
|---|---|---|
| [API reference](https://docs.typesafe.ai/api.md) | Official | Endpoint, request and response fields, status codes 401, 422, 429 and 529. No error body schema. |
| [Models](https://docs.typesafe.ai/models.md) | Official | Model aliases, token limits (64k tokens per request; 32k for `state` plus the longest question), rate limits, the `GET /v1/models` shape, `retry-after`. |
| [Confidence](https://docs.typesafe.ai/confidence.md) | Official | Exact `confidence` formulas for Choice and Score. Noul has no confidence. |
| [Primitives](https://docs.typesafe.ai/primitives.md) (and the `noul`, `choice`, `score` and `advanced` pages) | Official | Field meanings. `instructions` and criteria entries may be `null`. Examples show 2-decimal values. |
| [Python SDK reference](https://docs.typesafe.ai/sdk/python/api/retries) (v0.7.3, 2026-10-09) | Official SDK | Strict pydantic models that ignore extra fields. `usage` is a required response key. Reads `x-typesafe-request-id`. Retries 408, 429 and 5xx twice; 10 s timeout per attempt. Offers `extra_body` and `extra_headers`. |
| [JavaScript SDK reference](https://docs.typesafe.ai/sdk/javascript/api) | Official SDK | Same retry defaults and 10 000 ms timeout; honors `retry-after-ms`. `instructions` and Noul `criteria` are optional and nullable. |
| Unauthenticated probes of `https://api.typesafe.ai` (curl, 2026-10-13) | Observed | Status codes, error bodies and headers for missing keys, wrong methods and unknown paths. See [Probe results](#probe-results). |
| [DigitalOcean System One guide](https://docs.digitalocean.com/products/inference/how-to/use-system-one-api/) | Third party (reseller) | Same body shape, model ID `typesafe-jev-1.13.0`, same token limits. |
| [LiteLLM TypeSafe pass-through](https://docs.litellm.ai/docs/pass_through/typesafe.md) | Third party | Base URL `https://api.typesafe.ai`; `GET /v1/models` exists; aliases `jev-latest`, `jev-preview`, `jev-1.13.0`. |

No public OpenAPI document for the official API was found.

## What the gateway matches

| Area | Official behavior | Gateway |
|---|---|---|
| Endpoint | `POST /v1/systemone` | Same |
| Authentication | `Authorization: Bearer <key>`. No usable Bearer token (missing header, another scheme, empty token) gives **403**, message "Must supply an API key! Check your request and try again." An unknown key gives **401**, message "Cannot authenticate with the server. Please check your API key and try again." Both use `error_type` `authentication_error`. | Same statuses, messages and `error_type` when `GATEWAY_API_KEYS` is set. Auth is checked before the body, as on the official API. |
| Unknown path | 404 `{"detail":"Not Found"}` | Same `detail`, plus the gateway members described under [Error bodies](#error-bodies) |
| Wrong method | 405 `{"detail":"Method Not Allowed"}` with `Allow: POST` | Same |
| Request ID header | `x-typesafe-request-id` on every response | Present on every response (the value format differs, see [Request IDs](#request-ids)) |
| Request fields | `model`, `state`, `questions`; question `type` is `noul`, `choice` or `score`; `instructions` and `criteria` | Same fields and types. Question names are echoed back and never shown to the model. |
| Unknown fields | Ignored at the top level and inside questions (probe, 2026-10-20) | Ignored, except inside `user_instructions` |
| Answer shape | Noul `{type, noul}`; Choice `{type, choice, confidence, probabilities}`; Score `{type, score, confidence, legend, probabilities}` | Same fields in the same order |
| Answer order | `answers` and Choice `probabilities` follow the request order; Score levels are numbered `"0"`, `"1"`, ... | Same |
| `confidence` | Choice: `(p_max − 1/n) / (1 − 1/n)`. Score: `max(0, 1 − Σ pᵢ·abs(i − m) / MAD_unif)` with `MAD_unif = (1/n)·Σ abs(i − (n−1)/2)`, where `m` is the most likely level | Same formulas, computed from the normalized probabilities before rounding |
| Number precision | 2 decimals (probe) | Every number rounded to 2 decimals, so rounded probabilities need not sum to exactly 1 |
| `usage` | Required object with `input_tokens` and `output_tokens` | Always present; a count the upstream did not report is `null`, which the Python SDK accepts |
| `GET /v1/models` | `{"models":[{"name","description","release_date"}]}`, key required | Same shape for the configured public aliases. `description` and `release_date` come from route settings. Upstream model IDs are never listed. |
| Response `model` | The versioned ID that answered, even when the request used an alias | The route's `response_model` when set, else the requested alias. The README recommends a versioned `response_model` per route. |
| Score levels | Docs: 2 to 10. A 1-level Score was accepted in a probe and answered `score` 0, `confidence` 1. | 1 to 10 (`MAX_SCORE_LEVELS`); 1 level answers `score` 0, probability 1, `confidence` 1 |
| Choice options | At most 255 | At most 255 (`MAX_CHOICE_OPTIONS`) |
| Rate limits and overload | 429 with optional `retry-after`; 529 when overloaded | An upstream 429 or 529 keeps its status, and `retry-after` is forwarded. The gateway has no hosted rate limits of its own. |
| Streaming | Not supported | Not supported |

## Where the gateway differs on purpose

These differences are gateway extensions or gateway policy. Clients written for the official API can ignore the extensions, but the stricter rules can reject requests the official API would accept.

### Typed `user_instructions`

The gateway accepts an optional `user_instructions` object for bounded domain context. Its fields are fixed and typed (`domain`, `locale`, `objective`, `risk_posture`, `uncertainty_policy`, `terminology`, `constraints`, `reference_facts`, `decision_rules`, `tags`), each with its own size limits. An unknown field inside it fails the whole request with `422`. The contract is [`schema/user-instructions.schema.json`](../schema/user-instructions.schema.json).

The official API has no such field. It ignores unknown top-level fields, so a request carrying `user_instructions` is accepted there but the context is not used. Python clients can send it with `extra_body`; whether the JavaScript SDK can send extra body fields is not known.

### Question and state rules

The gateway requires real text in each question and in `state`. Text is counted in units: one per Han, Hiragana, Katakana or Thai character, otherwise one per run of word characters. `MIN_DESCRIPTION_WORDS` units (default 3, minimum 1) make text meaningful.

- Noul needs meaningful `instructions`, or a positive/negative `criteria` pair with two meaningful descriptions.
- Choice and Score need meaningful `instructions`, or a meaningful description for every option or level.
- `state` must not be `null`, a number, a boolean, or an array holding only those, and it needs at least `MIN_DESCRIPTION_WORDS` units.

A request that breaks a rule gets `422 invalid_request` naming the question or `state`. The full rules and examples are in the README section [Question and state rules](../README.md#question-and-state-rules).

These rules are stricter than anything seen on the official side. The only related rules observed came from the LiteLLM proxy's own request model, not from TypeSafe: it rejects a Noul with neither `instructions` nor `criteria`, a `state` that is `null`, a number or a boolean, and a Choice with no options.

### Noul positive/negative pairs

The official API documents only `true` and `false` as Noul `criteria` keys. The gateway also accepts these pairs, matched case-insensitively, and shows them to the model as `true`/`false`:

| Positive | Negative |
|---|---|
| `true` | `false` |
| `yes` | `no` |
| `agree` | `disagree` |
| `accept` | `reject` |
| `approve` | `deny` |
| `allow` | `block` |
| `pass` | `fail` |
| `positive` | `negative` |

The other pairs are not accepted on the path we tested. In the [test corpus](../examples/corpus/ANALYSIS.md) run of 2026-10-21, sent through a LiteLLM proxy, 22 of 50 requests returned 400 because of these keys, and all 50 passed after each pair was mapped to `true`/`false`. The proxy validates requests itself before forwarding them, so whether TypeSafe's API also rejects the keys is unverified. Clients that also call the official API should send `true`/`false`.

### Error bodies

Official error bodies hold only `detail`: `{"detail":{"error_type","message"}}` for auth errors, or `{"detail":"<text>"}` for 404 and 405. Gateway error bodies keep `detail` and add an `error` member (`code`, `message`, `request_id`) and a top-level `request_id`:

```json
{
  "detail": { "error_type": "authentication_error", "message": "Must supply an API key! Check your request and try again." },
  "error": { "code": "invalid_api_key", "message": "Must supply an API key! Check your request and try again.", "request_id": "gw_0000000000000001" },
  "request_id": "gw_0000000000000001"
}
```

For errors other than authentication, `detail.error_type` is the gateway `error.code`, because the official names are not known. Gateway errors also carry `X-System-One-Error-Source` and `X-System-One-Error-Code` headers. All of this is a deliberate choice; the official SDKs treat the body as opaque.

### Request IDs

The official API sends `x-typesafe-request-id: req_<32 hex>`. The gateway sends `gw_<16 hex>` and puts the same value in both `x-request-id` and `x-typesafe-request-id`. The gateway format was kept on purpose.

### Limits

The gateway applies its own limits. Most of them have no documented official counterpart, and the probes showed the official API accepts more.

| Limit | Gateway default | Official |
|---|---|---|
| Request body | 524,288 bytes (`MAX_BODY_BYTES`) | No byte limit documented. 64k tokens per request and 32k tokens for `state` plus the longest question. A `state` of about 100 KiB (17,680 input tokens) was accepted. |
| Questions | 32 (`MAX_QUESTIONS`) | Not documented; 33 were accepted |
| Question and option names | 128 characters (fixed) | Not documented; a 129-character question name was accepted |
| JSON depth | 32 (`MAX_JSON_DEPTH`) | Not documented; a `state` of depth 33 was accepted |
| Choice options | 255 | 255 documented; 256 was rejected by the LiteLLM proxy's request model |
| Score levels | 1 to 10 | 2 to 10 documented; 1 was accepted |

The gateway does not count tokens; the upstream provider's own limits apply. Billing, credits and hosted rate limits are not implemented.

### Other extensions and policies

- **Optional `model`.** The official API requires `model`. The gateway defaults it to `DEFAULT_JEV_MODEL` (`jev-latest`). The SDKs always send it.
- **`Idempotency-Key`.** The official API documents no idempotency. The gateway accepts an optional `Idempotency-Key` with a caller-scoped, process-local cache; replays carry `X-Idempotency-Replayed: true`. With the SDKs' 10 s timeout, a retry that arrives while the first attempt is still running gets `409 request_in_progress`, which the SDKs do not retry. See the README section [Using the official SDKs](../README.md#using-the-official-sdks).
- **`GET /openapi.json`.** An OpenAPI 3.1.0 description of the gateway routes, served without a token. The official API publishes none.
- **`GET /healthz`.** Process health, no token. The official API returns 404 for it.
- **`GET /api/system-one/config`.** Reports the default model and the gateway limits in effect, no token. The official API returns 404 for it.
- **Stricter input checks.** `415` for a non-JSON `Content-Type`; `400 invalid_json` for duplicate object keys and for malformed JSON. The official behavior for each is not settled.
- **Upstream failures.** An upstream 429 or 529 keeps its status and 5xx keep theirs. Every other upstream status, including 401, 403 and 404, becomes `502 provider_rejected_request`, so a caller never sees a provider's 4xx as its own. When no provider permit is free within `ADMISSION_WAIT_MS`, the gateway returns `503 provider_busy` where the official API would return 529; the SDKs retry both.
- **Errors from an upstream 200.** A refusal returns `422 provider_refused`; output cut off by the token limit returns `502 provider_output_truncated`.
- **Response headers.** `X-System-One-Response-Mode` on every answer, `Cache-Control: no-store` on every response. `X-Upstream-Provider`, `X-Upstream-Model` and `X-Upstream-Request-Id` name upstream providers and models, so they are off unless `EXPOSE_UPSTREAM_HEADERS=true`.
- **Calibration.** The gateway reproduces the wire contract and the meaning of each answer, not Jev's trained model. Probabilities and confidence depend on the configured upstream model.

## Probe results

### How the probes were run

- **2026-10-13, direct, no key.** curl requests to `https://api.typesafe.ai`. These established the auth statuses, the 404 and 405 bodies, the `Allow: POST` header, the `x-typesafe-request-id: req_<32 hex>` header on every response, and 404 for `/api/system-one/config` and `/healthz`.
- **2026-10-20, through a LiteLLM proxy.** Every probe except the authentication set went through a LiteLLM proxy's `/v1/systemone` endpoint with model `jev-1.13-free`. No probe got a 429, so the planned fallback to `jev-1.13` was not needed.
- **2026-10-20, direct, no key.** The authentication probes (missing header, `Basic x`, `Bearer` with an empty token, an invalid key, `GET /v1/models` without a key) went straight to `https://api.typesafe.ai`.

LiteLLM checks each request against its own request model before forwarding it. So 400 responses and their bodies, and some accept or reject results, may come from LiteLLM rather than TypeSafe. Only the direct probes are authoritative for status codes and headers. Below, "direct" means sent to `api.typesafe.ai`, "via proxy" means forwarded and answered by TypeSafe, and "proxy only" means rejected by LiteLLM before it reached TypeSafe.

### Confirmed

| What was sent | Path | Result |
|---|---|---|
| Choice and Score questions | Via proxy | `confidence` matches the published formulas: Choice (0.33, 0.66, 0.01) gives 0.49; Score (0, 0.01, 0.25, 0.71, 0.03) gives 0.75; Score (0.3, 0.5, 0.15, 0.05, 0) gives 0.54. Score `legend` is present. |
| Any successful call | Via proxy | Numbers are rounded to 2 decimals (`noul` 0.93, `score` 3.97, `confidence` 0.49). `usage` counts were present and never `null` in 24 successful responses. |
| Several questions and options | Via proxy | `answers` and Choice `probabilities` follow the request order, and fields inside each answer come in the order listed under [What the gateway matches](#what-the-gateway-matches). |
| A Score with 1 level | Via proxy | Accepted: `score` 0.0, `confidence` 1.0 |
| Unknown top-level and question fields | Via proxy | Ignored |
| 33 questions, a 129-character question name, `state` depth 33, a `state` of about 100 KiB (17,680 input tokens) | Via proxy | All accepted |
| Missing `Authorization`, `Basic x`, `Bearer` with an empty token | Direct | 403 `{"detail":{"error_type":"authentication_error","message":"Must supply an API key! Check your request and try again."}}` |
| A well-formed Bearer token with an invalid key | Direct | 401 with message "Cannot authenticate with the server. Please check your API key and try again." |
| `GET /v1/models` without a key | Direct | 403 |
| Error bodies and headers | Direct | Error bodies hold only `detail`; `x-typesafe-request-id` is `req_` plus 32 hex digits |

The gateway now matches all of these, except that it keeps its own request ID format, error members and stricter limits (see [Where the gateway differs on purpose](#where-the-gateway-differs-on-purpose)).

The unit test `map::tests::official_samples_round_to_two_decimals` recomputes the samples above: Choice (0.33, 0.66, 0.01) gives `confidence` 0.49; Score (0, 0.01, 0.25, 0.71, 0.03) gives `confidence` 0.75 and `score` 2.76; Score (0.3, 0.5, 0.15, 0.05, 0) gives `confidence` 0.54. From the 2-decimal probabilities shown, the expected level is 0.01 + 0.50 + 2.13 + 0.12 = 2.76. An official `score` of 2.77 next to these probabilities would mean TypeSafe also computes it from unrounded values, as the gateway does; the probes do not show those values.

### Rejected by the proxy only

These came from LiteLLM's request model and are not confirmed for TypeSafe:

- A Noul with neither `instructions` nor `criteria` ("A noul question requires instructions or criteria").
- A `state` that is `null`, a number or a boolean.
- A Choice with 0 options, and a Choice with 256 options (LiteLLM's maximum is 255).

### Not settled through the proxy

- **Duplicate JSON keys:** LiteLLM's parser accepted them (the last value wins).
- **`Content-Type: text/plain`:** accepted by LiteLLM.
- **`model: "jev-latest"`:** the proxy key had no access to it (403 `key_model_access_denied`), so the versioned response `model` was not observed.
- **`GET /v1/models`:** LiteLLM returned its own model list.

## Still unverified

- The official 422 body (including the field path format) and the 400 and 5xx bodies. The `error_type` names for errors other than authentication.
- 429 and 529 bodies: no probe hit either.
- Whether TypeSafe itself, rather than LiteLLM's request model, rejects a Noul with neither field, a scalar `state`, a Choice with 0 or 256 options, and Noul keys other than `true`/`false`.
- How the official API handles duplicate JSON keys, malformed JSON and a non-JSON `Content-Type`.
- The official limits on question count, name length, JSON depth and body size, beyond the values the probes showed as accepted.
- The versioned `model` returned for an alias such as `jev-latest`, and the exact official `/v1/models` entries.
- Whether official `usage` counts are ever `null`.
- Tie rules. The gateway takes the first most likely level for Score `confidence`, and the smallest option name (byte order) for Choice `choice`. No official tie was observed.
- Whether the JavaScript SDK can send `user_instructions` as an extra body field.
- Where jev2openaires, the project this gateway grew from, took its "System One native" limits (65,536 bytes, 32 questions, depth 32, 128 characters), its `X-System-One-Error-*` headers and `/api/system-one/config`. None of them appear in the official sources above.
