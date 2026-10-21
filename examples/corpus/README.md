# Decision-request test corpus

25 decision requests for `POST /v1/systemone`: 5 domains times 5 complexity levels. They are used to compare models and to grade answers. Every request has exactly three questions (one `noul`, one `choice`, one `score`) and a `user_instructions` block.

The request files carry no `model` field. The run scripts add it when they send, so the same files work for any route.

## Files

For each case `<domain>/L<n>`:

- `L<n>.json` is the request body that gets sent.
- `L<n>.meta.json` holds notes for people (what the case tests, which question depends on `user_instructions`). It is never sent.
- `L<n>.expected.json` holds one set of reference answers, never sent. Claude Opus 5.5 (medium effort) wrote them blind, from exactly the system prompt, user message and output schema the gateway sends to a Messages API upstream. They were then mapped back through the gateway (normalization, choice, score, confidence, 2-decimal rounding), so they compare number for number with gateway responses. Fields:
  - `with_user_instructions`: `answers` (response shape, keyed by your question names) and a one-line `reasons` entry per question.
  - `without_user_instructions`: the same, from a separate agent that only saw the request with `user_instructions` removed.
  - `ui_dependent_question`, `ui_dependence` and `ui_effect` (`with` / `without` answer for that question).
  - `note`, only where the raw reference output needed a repair (4 cases were missing a final `}`; values unchanged).

  Other answer sets sit beside it as `L<n>.expected.<set>.json`, with a `.no_user_instructions` variant.

The scripts and outputs in this directory:

- `run.sh` and `run.ps1` send the cases to one gateway route (see [Running](#running)); `.gitignore` keeps their `runs/` output out of git.
- `run_official.py` sends every case, with and without `user_instructions`, to an official-style `/v1/systemone` endpoint (for example LiteLLM). It mimics `user_instructions` as a "Domain context" block in each question's `instructions`, and maps noul criteria pairs to `true`/`false`, which the official API requires. It falls back through `--models` on a 429. Key from `JEV_KEY`, never printed.
- `run_async.py` sends every case, with and without `user_instructions`, to several gateway routes concurrently. After a failure, all workers pause and concurrency halves, with the pause growing while failures stay consecutive. Failed calls are retried, then recorded, so the run always finishes. `--only-failed` resends just the failures. Token from `JEV_TOKEN`.
- `results.html` is a single self-contained page charting `ANALYSIS.md`: agreement heatmap, levels, context flips, confidence, latency and split cases. Open it in any browser; it has no external dependencies, and its data is embedded from `analyse.py --json` plus the run logs.
- `analyse.py` compares every answer set present (`L<n>.expected[.<model>][.no_user_instructions].json`); `--sets a,b,...` limits it to those sets. [`ANALYSIS.md`](ANALYSIS.md) records the results.

## The grid

| Domain | L1 | L2 | L3 | L4 | L5 |
| --- | --- | --- | --- | --- | --- |
| `ecommerce` (returns and support) | Unopened kettle returned inside the window | Out-of-window laptop sleeve, loyal customer | Final-sale screen protector cracked by the customer | Non-delivery claim, carrier says delivered | Earbud serial mismatch with a warehouse log conflict (Chinese complaint) |
| `it_ops` (incidents and operations) | Wiki returns 503 for everyone | Replica disk at 94 percent at night | Latency jump after a deploy during a change freeze | Schema migration vs a sales event, backup unverified | Payment errors in one region (Japanese report) |
| `payments` (fraud and compliance) | Low-risk payment flagged by a postcode typo | Large electronics order from a new account abroad | Repeated just-under-limit transfers to a new receiver | Unauthorized-purchase dispute with a 3DS conflict | Merchant onboarding with an undeclared owner and a name match (Chinese registry) |
| `moderation` (user content) | Same promo link posted twelve times | Mildly rude general remark | Follower-selling post from a new account | Hostile comment in a heated exchange, warnings of different ages | Banter or attack, third-party reports only (Japanese appeal) |
| `insurance` (claims triage) | Cracked windshield, glass covered | Large water claim twelve days into the policy | Collision repair near the total-loss line | Laptop theft from a car, thin police report | Bakery fire with an unscheduled oven allegation (Chinese statement) |

## Complexity ladder

State, questions and `user_instructions` all grow with the level.

| Level | State | Questions | `user_instructions` fields |
| --- | --- | --- | --- |
| L1 | 1 to 2 plain sentences (a string) | Noul with instructions; choice with 3 options; score with 3 levels | `domain`, `objective` |
| L2 | Small flat object | Noul with instructions; choice with 4 options; score with 4 to 5 levels | plus `locale`, `risk_posture` |
| L3 | Nested object, several entities, irrelevant noise | Noul as a pair with no instructions; choice with labels only; score with 5 to 6 levels | plus `uncertainty_policy`, 1 to 2 `terminology` entries |
| L4 | Longer, partly contradictory or incomplete | Noul pair with no instructions; choice with 7 described options; score with 8 levels | plus 2 to 3 `constraints`, 1 to 2 `reference_facts` |
| L5 | Large nested object, conflicting signals, one prompt-injection line, a short Chinese or Japanese passage with real evidence | Noul (pair or instructions); choice with 11 options; score with 10 levels | plus 3 `decision_rules` with priorities, and `tags` |

In every case exactly one question (named in `ui_dependent_question` in the meta file) changes its answer if `user_instructions` is removed. The other two can be answered from the state alone. At L1 the dependence is deliberately weak: the question asks which kind of output the team needs, and only the `objective` field says.

## Gateway rules every case follows (P7)

Text is counted in units: each Han, Hiragana, Katakana or Thai character is one unit, and other text counts word runs. Text is "meaningful" at 3 or more units.

- Noul: meaningful `instructions`, or `criteria` as one positive/negative pair with both descriptions meaningful. Pairs: true/false, yes/no, agree/disagree, accept/reject, approve/deny, allow/block, pass/fail, positive/negative.
- Choice: 1 to 255 options, with meaningful `instructions` or every option description meaningful.
- Score: 1 to 10 levels, with meaningful `instructions` or every level description meaningful.
- State: an object, an array with text, or a string, with at least 3 units of text in its keys or string values. Not null, a bare number, or a boolean.
- Depth at most 32, question names at most 128 characters, at most 32 questions.

See "Question and state rules" in the main README for the full text, and `schema/user-instructions.schema.json` for `user_instructions`.

## Running

Both scripts send every case (or one filtered case set), save one `<domain>-L<n>.response.json` per case in the output directory, print one line per case, and finish with a totals table (ok, 4xx, 5xx, no connect, failure rate). Each saved file holds the HTTP status, the `x-request-id` and `x-system-one-response-mode` headers, the latency and the response body.

The token comes from the `JEV_TOKEN` environment variable and goes out as `Authorization: Bearer`. If it is unset, no auth header is sent. The scripts never print it. 4xx and 5xx responses count as results; the exit code is nonzero only when a request could not connect.

Defaults: base URL `http://127.0.0.1:8080`, model `jev-latest`, output `./runs/<model>-<timestamp>`. The filter is a domain (`payments`), a level (`L3`) or both (`moderation/L5`).

Bash (needs `curl` and `python3` or `python`):

```bash
# Local gateway, all 25 cases
./run.sh

# Local gateway, one model, one domain
./run.sh -m jev-claude -f ecommerce

# Remote host with a token
JEV_TOKEN=... ./run.sh -u https://gateway.example.com -m jev-latest -o ./runs/remote-check
```

PowerShell 7:

```powershell
# Local gateway, all 25 cases
./run.ps1

# Local gateway, one case
./run.ps1 -Model jev-claude -Filter it_ops/L4

# Remote host with a token
$env:JEV_TOKEN = '...'
./run.ps1 -BaseUrl https://gateway.example.com -Model jev-latest -OutDir ./runs/remote-check
```

Everything under `runs/` is ignored by git.
