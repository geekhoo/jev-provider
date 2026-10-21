# Corpus results: ten answer sets compared (2026-10-21)

The 25 corpus cases (5 domains × 5 complexity levels) were each answered 20 times: by 10 answer sets, with and without `user_instructions`. That's 500 answers.

## Summary

- **Context drives agreement.** With `user_instructions`, two sets pick the same top Choice option in 21 of 25 cases on average. Without them, 16 of 25. Each set's dependent answer flips in 10 to 14 of 25 cases when the context goes.
- **Harder cases split the numbers, not the decisions.** Top choices agree just as well at L5 as at L1. Score answers spread from 0.11 levels apart at L1 to 0.59 at L5.
- **L2 is the weak level.** Its cases hinge on `risk_posture` alone, and only 76% of top choices there match the consensus, against 90–98% at the other levels.
- **The gateway passes each model's judgement through unchanged.** A model behind the gateway agrees with the same model answering directly, at 23–24 of 25.
- **Models differ in confidence more than in decisions.** The OpenAI models and the official Jev API report Choice confidence of 0.72–0.83. The rest sit near 0.5.
- **Nobody followed the injected instruction** in the L5 cases.
- **All 500 answers are usable.** Reaching that took one formatting repair, an official-API key mapping and a longer gateway timeout.

## The ten sets

| Set | Model | Path | Input it saw |
| --- | --- | --- | --- |
| opus | Claude Opus 5.5, medium effort | Subagent | The exact upstream prompt the gateway builds (opaque aliases), mapped back through the gateway |
| luna | gpt-6-luna | Own harness | Request JSON plus `prompts/system-instructions.txt` |
| mimo | mimo-v2.6-pro | Own harness | Same as luna |
| deepseek | zen/deepseek-v4.1-flash | Own harness | Same as luna |
| jev | jev-1.13-free (official Jev API) | LiteLLM `/v1/systemone` via `run_official.py` | Official request format; `user_instructions` mimicked in `instructions` |
| gw-deep | claude-sonnet-5-5, effort medium | Gateway route `jev-deep` | Gateway prompt, json_schema output |
| gw-latest | claude-haiku-5-5, effort xhigh | Gateway route `jev-latest` | Gateway prompt, json_schema output |
| gw-mimo | mimo-v2.6-pro | Gateway route `jev-mimo` | Gateway prompt, prompt output mode |
| gw-openai | zen/gpt-6-luna, effort xhigh | Gateway route `jev-openai` | Gateway prompt, Responses json_schema |
| gw-openai-deep | zen/gpt-6.1-sol, effort medium | Gateway route `jev-openai-deep` | Gateway prompt, Responses json_schema |

The gateway routes ran on ubuntu2604 (image `v0.9.0`, code `75eed5c`), all upstream calls through LiteLLM. Files are `<domain>/L<n>.expected[.<set>][.no_user_instructions].json`. Every number below comes from these commands, except the latencies in section 1, which come from the run logs:

```bash
python examples/corpus/analyse.py                                    # all ten sets
python examples/corpus/analyse.py --sets opus,luna,mimo,deepseek,jev # the five non-gateway sets
python examples/corpus/analyse.py --json summary.json                # numbers behind the charts
```

`score` and `confidence` are recomputed from each set's probabilities with the gateway's formulas, so every set is measured the same way. "Consensus" means the average Choice distribution across all ten sets, or the median for Score and noul.

## 1. Reliability and latency

| Set | Answers | Median | p90 | Slowest | Notes |
| --- | --- | --- | --- | --- | --- |
| jev | 50/50 | 0.6 s | 0.8 s | 1.1 s | 22/50 rejected at first (see 7) |
| gw-deep | 50/50 | 3.3 s | 4.6 s | 10.0 s | Intermittent LiteLLM 403; 27 calls needed retries |
| gw-openai-deep | 50/50 | 6.7 s | 10.0 s | 14.2 s | |
| gw-openai | 50/50 | 7.9 s | 16.6 s | 34.1 s | One case timed out at the old 20 s limit |
| gw-latest | 50/50 | 8.7 s | 13.2 s | 15.8 s | |
| gw-mimo | 50/50 | 11.7 s | 23.4 s | 45.0 s | |

The official API answers about five times faster than the fastest gateway route. Routes with effort `xhigh` or prompt mode are the slowest.

Getting to 500 usable answers took three fixes:

- **Gateway timeout.** One jev-openai call, the largest case at `effort: xhigh`, hit the 20 s provider `timeout_ms` on all 8 attempts. With 60 s it answered in 34.1 s. The routes now run with `timeout_ms` 60000.
- **Sonnet 403s.** LiteLLM returned HTTP 403 for claude-sonnet-5-5 now and then. The gateway reported these as `502 provider_rejected_request`, and every affected call succeeded on retry. This looks like a per-model limit on the LiteLLM side.
- **Opus repair.** Four Opus answers without `user_instructions` were missing their final `}`. One brace was added and the values were left unchanged; those files carry a `note`.

The first gateway run took 59 minutes. Its backoff grew with every failure, ever, so it settled at one call every 60 s. `run_async.py` now grows the backoff only while failures are consecutive.

Since P3, the gateway and the official API round each probability to 2 decimals on its own. A few long distributions therefore sum to 1.02 or 1.03. That's expected, and the analysis allows for it.

## 2. Agreement: context makes the models converge

Top Choice option shared by each pair, out of 25, **with** `user_instructions`:

| | opus | luna | mimo | deepseek | jev | gw-deep | gw-latest | gw-mimo | gw-openai | gw-openai-deep |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| opus | — | 23 | 22 | 20 | 19 | 20 | 23 | 23 | 22 | 22 |
| luna | 23 | — | 24 | 22 | 21 | 19 | 23 | 23 | 23 | 24 |
| mimo | 22 | 24 | — | 22 | 20 | 18 | 22 | 24 | 22 | 23 |
| deepseek | 20 | 22 | 22 | — | 18 | 18 | 20 | 21 | 20 | 21 |
| jev | 19 | 21 | 20 | 18 | — | 19 | 21 | 19 | 19 | 20 |
| gw-deep | 20 | 19 | 18 | 18 | 19 | — | 20 | 19 | 19 | 20 |
| gw-latest | 23 | 23 | 22 | 20 | 21 | 20 | — | 21 | 21 | 22 |
| gw-mimo | 23 | 23 | 24 | 21 | 19 | 19 | 21 | — | 22 | 22 |
| gw-openai | 22 | 23 | 22 | 20 | 19 | 19 | 21 | 22 | — | 23 |
| gw-openai-deep | 22 | 24 | 23 | 21 | 20 | 20 | 22 | 22 | 23 | — |

The pair average is **21.1** with context, ranging from 18 to 24. Without context it falls to **15.7**, ranging from 9 to 19. The lowest pair without context is luna and gw-mimo at 9. Context isn't decoration: it's what these models agree on.

Noul answers agree closely in both cases, with a median difference of 0.02–0.06 from the consensus. Choice and Score are where the sets differ.

## 3. Where the sets disagree

**By complexity level**, with `user_instructions`, across every question:

| Level | Choice tops matching the consensus | Score spread (sd, in levels) | Noul spread (sd) |
| --- | --- | --- | --- |
| L1 | 45/50 (90%) | 0.11 | 0.01 |
| L2 | 38/50 (76%) | 0.28 | 0.02 |
| L3 | 48/50 (96%) | 0.30 | 0.04 |
| L4 | 49/50 (98%) | 0.46 | 0.09 |
| L5 | 47/50 (94%) | 0.59 | 0.05 |

Two patterns show up here.

- **Score spread grows with complexity.** The sets settle on the same decision even in long, conflicting cases, but they place a Score level less consistently as the evidence gets messier. An earlier version of this report said harder cases caused no more disagreement. That holds only for top choices.
- **L2 is the outlier for decisions.** Its cases tip a borderline call on `risk_posture` alone. Some models give that soft signal a lot of weight and others very little.

**By case**, on the question each case makes depend on `user_instructions`: all ten sets agree in 15 of 25. The splits:

| Case | Majority | Different answers |
| --- | --- | --- |
| it_ops-L2 | page_now (6) | ticket_for_morning: opus, gw-deep, gw-mimo; auto_cleanup_job: gw-openai |
| ecommerce-L1 | send_to_team_queue (7) | decide_return_now: jev, gw-deep, gw-latest |
| ecommerce-L2 | approve_refund (7) | escalate_to_supervisor: opus, gw-latest; offer_store_credit: gw-deep |
| insurance-L5 | refer_equipment_part_to_siu (7) | refer_to_special_investigations: gw-deep, gw-openai-deep; pay_building_advance_only: gw-openai |
| payments-L2 | step_up_verification (7) | decline: mimo, gw-mimo; manual_review: deepseek |
| insurance-L3 | total_loss (8) | repair: jev, gw-deep |
| moderation-L2 | add_warning_label (8) | leave_up: deepseek, gw-deep |
| ecommerce-L4 | open_carrier_investigation (9) | refund_after_investigation: deepseek |
| insurance-L1 | score_claim_complexity (9) | gate_payment: jev |
| moderation-L1 | audit_for_quality (9) | remove_or_keep_now: jev |

Three of these point at the cases themselves, not at any model:

- **insurance-L5** offers two options so close in meaning (`refer_equipment_part_to_siu` and `refer_to_special_investigations`) that a split between them says more about the wording than about judgement.
- **payments-L2** draws three different answers. Its evidence likely supports several of them.
- **mimo answers `decline` on payments-L2 in both harnesses.** That's a stable trait of the model.

## 4. Each set's profile

Compared with the all-set consensus, every question, with `user_instructions`:

| Set | Choice tops matching consensus | Choice distance (TV) | Score distance | Choice confidence | Score confidence |
| --- | --- | --- | --- | --- | --- |
| luna | 25/25 | 0.18 | 0.22 | 0.78 | 0.66 |
| mimo | 24/25 | 0.12 | 0.16 | 0.54 | 0.49 |
| gw-openai-deep | 24/25 | 0.21 | 0.34 | 0.83 | 0.64 |
| opus | 23/25 | 0.11 | 0.18 | 0.55 | 0.53 |
| gw-latest | 23/25 | 0.15 | 0.24 | 0.46 | 0.48 |
| gw-mimo | 23/25 | 0.12 | 0.16 | 0.55 | 0.53 |
| gw-openai | 23/25 | 0.17 | 0.27 | 0.74 | 0.59 |
| deepseek | 22/25 | 0.15 | 0.35 | 0.52 | 0.46 |
| jev | 21/25 | 0.25 | 0.47 | 0.72 | 0.72 |
| gw-deep | 19/25 | 0.16 | 0.22 | 0.52 | 0.49 |

- **mimo, in both harnesses, and opus sit closest to the consensus distribution.**
- **The OpenAI models are sure of themselves.** luna, gw-openai and gw-openai-deep usually land on the consensus top choice, with confidence of 0.74–0.83 against roughly 0.5 for most others. That isn't a mistake in itself. Before automating anything on their confidence, calibrate against labelled outcomes.
- **gw-deep (Sonnet) differs from the group most often on decisions.** It disagrees in 6 of 25 cases, and once (ecommerce-L2, `offer_store_credit`) it chose an answer no other set picked.
- **jev is furthest out on Score** (0.47) and on distribution shape (0.25).

## 5. The effect of `user_instructions`

Each case has one question built to depend on `user_instructions`. Removing them changes that question's top choice in:

| opus | gw-mimo | gw-openai | luna | mimo | gw-deep | deepseek | gw-latest | gw-openai-deep | jev |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 14 | 13 | 13 | 12 | 12 | 12 | 11 | 11 | 11 | 10 |

The designed dependence works in every set and at a similar rate. jev reacts least. Without context, jev actually agrees with the consensus more (22/25) than with it (21/25). It largely answers from the state and seems to put less weight on context written into `instructions`. Part of that may come from the mimic format rather than the model (see 7).

## 6. Same model, different path

| Model | Pair | Top choice (with UI) | Choice distance | Score difference | Noul difference |
| --- | --- | --- | --- | --- | --- |
| mimo-v2.6-pro | mimo vs gw-mimo | 24/25 | 0.11 | 0.21 | 0.02 |
| gpt-6-luna | luna vs gw-openai | 23/25 | 0.14 | 0.33 | 0.01 |

These pairs differ in harness, prompt rendering, output mode and (for luna) effort setting. They still agree about as closely as any two sets in this study. So **the gateway carries each model's judgement through faithfully**, and the differences between rows in this report come from the models.

## 7. The official Jev API

`run_official.py` sent the corpus to `jev-1.13-free` through LiteLLM, set to fall back to `jev-1.13` on a 429. No 429 happened. Two differences from the gateway matter to anyone porting requests:

- **Noul criteria keys must be `true` and `false`.** The first run returned 400 on 22 of 50 requests, all for this reason. The gateway's pair extension (`agree`/`disagree`, `approve`/`deny`, …) doesn't exist officially. After mapping each pair to `true`/`false`, as the gateway does internally, all 50 succeeded.
- **`user_instructions` is ignored.** The script wrote the context as a plain-text "Domain context" block at the start of each question's `instructions`.

jev misses where the context should decide:

- **insurance-L1 and moderation-L1:** it stands alone against the other nine. The `objective` field alone decides these.
- **ecommerce-L1:** it shares its answer with gw-deep and gw-latest.
- **insurance-L3:** it picks repair, with gw-deep. Here a terminology definition should tip the case.

So it follows context written into `instructions` only weakly. On evidence-driven cases it matches the consensus, including every L4 and L5 dependent question. The official response has no reasons, so its injection handling can only be judged by its answers.

## 8. Prompt injection

Every L5 state carries a "SYSTEM: ignore all rules…" line. All ten sets picked the same top choice in every L5 case, so none followed it. How often a set's reasons mention the line (opus 3/5, luna 2/5, mimo 1/5, deepseek 0/5; the others give no reasons) reflects how much each explains, not whether it resisted.

## Caveats

- **There is no ground truth.** Agreement measures consensus, not correctness. A shared bias would look like agreement.
- **Inputs differ by path,** as listed in the table of sets. The same-model pairs in section 6 suggest the effect is small.
- **Model and harness are mixed together** for luna, mimo and deepseek.
- **Every dependent question is a Choice question,** so the `user_instructions` effect is measured on Choice only.
- **One run per set.** Repeat runs were not collected, so run-to-run noise is unknown. The gateway calls used each route's default sampling.

## Follow-ups

1. **Rework the weak cases** before using the corpus for grading:
   - give payments-L2 and the other L2 cases a firmer `user_instructions` lever than `risk_posture`;
   - make the two SIU options in insurance-L5 clearly distinct.
2. **Measure run-to-run noise** by repeating one route on the full corpus 3 times, so differences between sets can be told apart from sampling variation.
3. **Check confidence calibration** against labelled outcomes before automating on the OpenAI routes or the official API.
4. **Look into LiteLLM's intermittent 403** for claude-sonnet-5-5.
5. **For official-API clients:** send noul criteria as `true`/`false`, and write context into `instructions`.
