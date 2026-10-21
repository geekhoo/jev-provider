use crate::error::GatewayError;
use crate::model::*;
use hyper::StatusCode;
use serde_json::Value;

pub fn map_output(
    req: &JevRequest,
    prepared: &PreparedDecision,
    output: &Value,
    model: String,
    usage: Option<Usage>,
) -> Result<JevResponse, GatewayError> {
    let raw_answers = output
        .get("answers")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid_provider("missing object field 'answers'"))?;
    // Answers follow request order; prepared questions are sorted by name.
    let mut answers = Vec::with_capacity(req.questions.len());

    for (request_id, question) in &req.questions {
        let q = prepared
            .questions
            .iter()
            .find(|q| q.request_id == *request_id)
            .ok_or_else(|| invalid_provider("question mapping disappeared"))?;
        let raw = raw_answers
            .get(&q.internal_id)
            .and_then(Value::as_object)
            .ok_or_else(|| invalid_provider(format!("missing answer for {}", q.request_id)))?;
        match q.kind {
            PreparedKind::Noul => {
                let p = raw
                    .get("probability")
                    .and_then(Value::as_f64)
                    .ok_or_else(|| {
                        invalid_provider(format!("invalid noul probability for {}", q.request_id))
                    })?;
                answers.push((
                    q.request_id.clone(),
                    JevAnswer::Noul {
                        noul: round2(p.clamp(0.0, 1.0)),
                    },
                ));
            }
            PreparedKind::Choice => {
                let (dist, normalized) = read_distribution(q, raw)?;
                let names = q.options.iter().map(|o| o.request_key.as_str());
                let choice = argmax(names.zip(normalized.iter().copied()))
                    .ok_or_else(|| invalid_provider("empty choice distribution"))?
                    .to_owned();
                let confidence = choice_confidence(&normalized);
                let JevQuestion::Choice { criteria, .. } = question else {
                    return Err(invalid_provider("choice question mapping type mismatch"));
                };
                // Options in request order; prepared options are sorted by name.
                let probabilities = OrderedMap(
                    criteria
                        .keys()
                        .map(|k| (k.clone(), dist.get(k).copied().unwrap_or(0.0)))
                        .collect(),
                );
                answers.push((
                    q.request_id.clone(),
                    JevAnswer::Choice {
                        choice,
                        confidence,
                        probabilities,
                    },
                ));
            }
            PreparedKind::Score => {
                let (dist, normalized) = read_distribution(q, raw)?;
                let confidence = score_confidence(&normalized);
                let score = round2(
                    normalized
                        .iter()
                        .enumerate()
                        .map(|(i, p)| i as f64 * p)
                        .sum(),
                );
                let JevQuestion::Score { criteria, .. } = question else {
                    return Err(invalid_provider("score question mapping type mismatch"));
                };
                let legend = OrderedMap(
                    criteria
                        .iter()
                        .enumerate()
                        .map(|(i, v)| (i.to_string(), v.clone()))
                        .collect(),
                );
                answers.push((
                    q.request_id.clone(),
                    JevAnswer::Score {
                        score,
                        confidence,
                        legend,
                        probabilities: dist,
                    },
                ));
            }
        }
    }
    Ok(JevResponse {
        model,
        answers: OrderedMap(answers),
        usage: usage.unwrap_or_default(),
    })
}

/// Returns the caller-keyed distribution rounded to 2 decimals and the
/// normalized, unrounded probabilities, both in option order. Confidence,
/// score and the chosen option come from the unrounded values, so the rounded
/// ones need not sum to 1 (official behavior, D23).
fn read_distribution(
    q: &PreparedQuestion,
    raw: &serde_json::Map<String, Value>,
) -> Result<(OrderedMap<f64>, Vec<f64>), GatewayError> {
    let probs = raw
        .get("probabilities")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid_provider(format!("missing probabilities for {}", q.request_id)))?;
    let mut values = Vec::with_capacity(q.options.len());
    let mut sum = 0.0;
    for o in &q.options {
        let v = probs
            .get(&o.internal_key)
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let safe = if v.is_finite() && v > 0.0 { v } else { 0.0 };
        values.push(safe);
        sum += safe;
    }
    if sum.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater) {
        let uniform = 1.0 / q.options.len().max(1) as f64;
        values.fill(uniform);
        sum = 1.0;
    }
    for v in &mut values {
        *v /= sum;
    }
    let out = OrderedMap(
        q.options
            .iter()
            .zip(&values)
            .map(|(o, v)| (o.request_key.clone(), round2(*v)))
            .collect(),
    );
    Ok((out, values))
}

/// Official Choice confidence: `(p_max - 1/n) / (1 - 1/n)`.
fn choice_confidence(probs: &[f64]) -> f64 {
    let n = probs.len() as f64;
    if probs.len() <= 1 {
        return 1.0;
    }
    let p_max = probs.iter().copied().fold(0.0, f64::max);
    round2(((p_max - 1.0 / n) / (1.0 - 1.0 / n)).clamp(0.0, 1.0))
}

/// Official Score confidence: `max(0, 1 - MAD_m / MAD_unif)`, where `MAD_m` is
/// the mean absolute level distance from the most likely level `m` (the first
/// maximum in level order) and `MAD_unif` is the uniform distribution's mean
/// absolute deviation from the center level.
fn score_confidence(probs: &[f64]) -> f64 {
    let n = probs.len();
    // One level: MAD_unif is 0; the official answer is confidence 1 (P5).
    if n <= 1 {
        return 1.0;
    }
    let mut m = 0;
    for (i, p) in probs.iter().enumerate() {
        if *p > probs[m] {
            m = i;
        }
    }
    let mad: f64 = probs
        .iter()
        .enumerate()
        .map(|(i, p)| p * i.abs_diff(m) as f64)
        .sum();
    let center = (n - 1) as f64 / 2.0;
    let mad_unif = (0..n).map(|i| (i as f64 - center).abs()).sum::<f64>() / n as f64;
    round2((1.0 - mad / mad_unif).clamp(0.0, 1.0))
}

/// Highest probability; on a tie the smallest option name in byte order.
fn argmax<'a>(dist: impl Iterator<Item = (&'a str, f64)>) -> Option<&'a str> {
    dist.max_by(|a, b| a.1.total_cmp(&b.1).then_with(|| b.0.cmp(a.0)))
        .map(|(k, _)| k)
}

/// Official TypeSafe precision: 2 decimal places (D23).
fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

fn invalid_provider(message: impl Into<String>) -> GatewayError {
    GatewayError::local(
        StatusCode::BAD_GATEWAY,
        "invalid_provider_response",
        message,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::prepare_decision;
    use serde_json::json;
    use std::collections::HashMap;

    #[test]
    fn invalid_weights_fall_back_to_uniform_distribution() {
        let req: JevRequest = serde_json::from_value(json!({
            "state":"x",
            "questions":{"s":{"type":"score","criteria":["low","high"]}}
        }))
        .unwrap();
        let prepared = prepare_decision(&req).unwrap();
        let output = json!({"answers":{"q0":{"probabilities":{"l0":-1.0,"l1":null}}}});
        let response = map_output(&req, &prepared, &output, "jev-latest".into(), None).unwrap();
        match response.answers.get("s").unwrap() {
            JevAnswer::Score {
                score,
                probabilities,
                confidence,
                ..
            } => {
                assert_eq!(*score, 0.5);
                assert_eq!(probabilities["0"], 0.5);
                assert_eq!(probabilities["1"], 0.5);
                assert_eq!(*confidence, 0.0);
            }
            _ => panic!("expected score answer"),
        }
    }

    #[test]
    fn choice_confidence_matches_official_vectors() {
        let third = 1.0 / 3.0;
        let cases: &[(&[f64], f64)] = &[
            (&[0.8, 0.2], 0.6),
            (&[0.61, 0.35, 0.04], 0.42),
            (&[1.0 / 6.0, 2.0 / 6.0, 3.0 / 6.0], 0.25),
            (&[third, third, third], 0.0),
            (&[1.0], 1.0),
        ];
        for (probs, expected) in cases {
            assert_eq!(choice_confidence(probs), *expected, "{probs:?}");
        }
    }

    #[test]
    fn score_confidence_matches_official_vectors() {
        let cases: &[(&[f64], f64)] = &[
            (&[0.5, 0.0, 0.5], 0.0),
            (&[0.0, 0.57, 0.43], 0.36),
            (&[0.0, 0.5, 0.5], 0.25),
            (&[0.1, 0.3, 0.6], 0.25),
            (&[1.0 / 6.0, 2.0 / 6.0, 3.0 / 6.0], 0.0),
            // D12: the first maximum in level order is `m` (0.1 with m = 1).
            (&[0.4, 0.4, 0.2], 0.0),
            (&[1.0], 1.0),
        ];
        for (probs, expected) in cases {
            assert_eq!(score_confidence(probs), *expected, "{probs:?}");
        }
    }

    #[test]
    fn score_confidence_uses_level_order() {
        let req: JevRequest = serde_json::from_value(json!({
            "state":"x",
            "questions":{"s":{"type":"score","criteria":["low","mid","high"]}}
        }))
        .unwrap();
        let p = prepare_decision(&req).unwrap();
        let out = json!({"answers":{"q0":{"probabilities":{"l0":0.0,"l1":57.0,"l2":43.0}}}});
        let r = map_output(&req, &p, &out, "jev-latest".into(), None).unwrap();
        match r.answers.get("s").unwrap() {
            JevAnswer::Score { confidence, .. } => assert_eq!(*confidence, 0.36),
            _ => panic!("expected score answer"),
        }
    }

    #[test]
    fn usage_is_always_serialized_with_null_counts() {
        let req: JevRequest = serde_json::from_value(json!({
            "state":"x",
            "questions":{"n":{"type":"noul"}}
        }))
        .unwrap();
        let p = prepare_decision(&req).unwrap();
        let out = json!({"answers":{"q0":{"probability":0.7}}});

        let r = map_output(&req, &p, &out, "jev-latest".into(), None).unwrap();
        let body = serde_json::to_value(&r).unwrap();
        assert_eq!(
            body["usage"],
            json!({"input_tokens":null,"output_tokens":null})
        );
        assert_eq!(body["answers"]["n"], json!({"type":"noul","noul":0.7}));

        let partial = Usage {
            input_tokens: Some(12),
            output_tokens: None,
        };
        let r = map_output(&req, &p, &out, "jev-latest".into(), Some(partial)).unwrap();
        let body = serde_json::to_value(&r).unwrap();
        assert_eq!(
            body["usage"],
            json!({"input_tokens":12,"output_tokens":null})
        );
    }

    #[test]
    fn serializes_in_request_order_with_official_field_order() {
        let levels: Vec<String> = (0..11).map(|i| format!("level {i}")).collect();
        // Parsed from text: `json!` would sort the keys before deserializing.
        let text = format!(
            r#"{{"state":"x","questions":{{
                "zeta":{{"type":"score","criteria":{}}},
                "Alpha":{{"type":"choice","criteria":{{"tech":"t","billing":"b","Zoo":"z","account":"a"}}}},
                "mid":{{"type":"noul"}}
            }}}}"#,
            serde_json::to_string(&levels).unwrap()
        );
        let req: JevRequest = serde_json::from_str(&text).unwrap();
        let p = prepare_decision(&req).unwrap();
        // Internal IDs and aliases stay sorted by name: Alpha = q0 with
        // options Zoo, account, billing, tech = o0..o3; mid = q1; zeta = q2.
        let ids: HashMap<&str, &str> = p
            .questions
            .iter()
            .map(|q| (q.request_id.as_str(), q.internal_id.as_str()))
            .collect();
        assert_eq!((ids["Alpha"], ids["mid"], ids["zeta"]), ("q0", "q1", "q2"));
        let score_probs: serde_json::Map<String, Value> =
            (0..11).map(|i| (format!("l{i}"), json!(1.0))).collect();
        let out = json!({"answers":{
            "q0": {"probabilities":{"o0":0.1,"o1":0.2,"o2":0.3,"o3":0.4}},
            "q1": {"probability":0.5},
            "q2": {"probabilities": score_probs}
        }});
        let r = map_output(&req, &p, &out, "jev-latest".into(), None).unwrap();
        let body = serde_json::to_string(&r).unwrap();

        // Score: levels in numeric order ("10" follows "9"), official field order.
        let entries = |value: &dyn Fn(usize) -> String| {
            (0..11)
                .map(|i| format!("\"{i}\":{}", value(i)))
                .collect::<Vec<_>>()
                .join(",")
        };
        let zeta = format!(
            r#""zeta":{{"type":"score","score":5.0,"confidence":0.0,"legend":{{{}}},"probabilities":{{{}}}}}"#,
            entries(&|i| format!("\"level {i}\"")),
            entries(&|_| "0.09".into())
        );
        // Choice: options in request order, official field order.
        let alpha = r#""Alpha":{"type":"choice","choice":"tech","confidence":0.2,"probabilities":{"tech":0.4,"billing":0.3,"Zoo":0.1,"account":0.2}}"#;
        let mid = r#""mid":{"type":"noul","noul":0.5}"#;
        // Questions in request order.
        assert!(
            body.starts_with(&format!(
                r#"{{"model":"jev-latest","answers":{{{zeta},{alpha},{mid}}},"usage""#
            )),
            "{body}"
        );
    }

    /// 8B probe samples: official TypeSafe answers rounded to 2 decimals.
    #[test]
    fn official_samples_round_to_two_decimals() {
        let req: JevRequest = serde_json::from_value(json!({
            "state":"x",
            "questions":{
                "c":{"type":"choice","criteria":{"a":"a","b":"b","c":"c"}},
                "n":{"type":"noul"},
                "s":{"type":"score","criteria":["0","1","2","3","4"]}
            }
        }))
        .unwrap();
        let p = prepare_decision(&req).unwrap();
        let out = json!({"answers":{
            "q0":{"probabilities":{"o0":0.33,"o1":0.66,"o2":0.01}},
            "q1":{"probability":0.931},
            "q2":{"probabilities":{"l0":0.0,"l1":0.01,"l2":0.25,"l3":0.71,"l4":0.03}}
        }});
        let r = map_output(&req, &p, &out, "jev-latest".into(), None).unwrap();
        let body = serde_json::to_value(&r).unwrap();
        assert_eq!(
            body["answers"]["c"],
            json!({"type":"choice","choice":"b","confidence":0.49,
                   "probabilities":{"a":0.33,"b":0.66,"c":0.01}})
        );
        assert_eq!(body["answers"]["n"], json!({"type":"noul","noul":0.93}));
        // Score from these 2-dp inputs is 2.76 (0.01 + 0.50 + 2.13 + 0.12).
        let s = &body["answers"]["s"];
        assert_eq!(s["score"], json!(2.76));
        assert_eq!(s["confidence"], json!(0.75));

        // Confidence comes from the unrounded values; rounded ones need not sum to 1.
        let third = 1.0 / 3.0;
        let out = json!({"answers":{
            "q0":{"probabilities":{"o0":third,"o1":third,"o2":third}},
            "q1":{"probability":0.5},
            "q2":{"probabilities":{"l0":0.3,"l1":0.5,"l2":0.15,"l3":0.05,"l4":0.0}}
        }});
        let r = map_output(&req, &p, &out, "jev-latest".into(), None).unwrap();
        let body = serde_json::to_value(&r).unwrap();
        assert_eq!(
            body["answers"]["c"]["probabilities"],
            json!({"a":0.33,"b":0.33,"c":0.33})
        );
        assert_eq!(body["answers"]["c"]["confidence"], json!(0.0));
        assert_eq!(body["answers"]["s"]["confidence"], json!(0.54));
        assert_eq!(body["answers"]["s"]["score"], json!(0.95));
    }

    /// One Score level (P5): score 0, probability 1, confidence 1 (official).
    #[test]
    fn single_level_score_is_certain() {
        let req: JevRequest = serde_json::from_value(json!({
            "state":"x",
            "questions":{"s":{"type":"score","criteria":["only"]}}
        }))
        .unwrap();
        let p = prepare_decision(&req).unwrap();
        assert_eq!(
            p.schema["properties"]["answers"]["properties"]["q0"]["properties"]["probabilities"]
                ["required"],
            json!(["l0"])
        );
        for weight in [json!(0.75), json!(0.0), json!(null)] {
            let out = json!({"answers":{"q0":{"probabilities":{"l0":weight}}}});
            let r = map_output(&req, &p, &out, "jev-latest".into(), None).unwrap();
            assert_eq!(
                serde_json::to_value(&r).unwrap()["answers"]["s"],
                json!({"type":"score","score":0.0,"confidence":1.0,
                       "legend":{"0":"only"},"probabilities":{"0":1.0}}),
                "{weight}"
            );
        }
    }

    #[test]
    fn choice_ties_pick_the_smallest_option_name() {
        let req: JevRequest = serde_json::from_value(json!({
            "state":"x",
            "questions":{"c":{"type":"choice","criteria":{"b":"b","a":"a","c":"c"}}}
        }))
        .unwrap();
        let p = prepare_decision(&req).unwrap();
        let out = json!({"answers":{"q0":{"probabilities":{"o0":0.2,"o1":0.4,"o2":0.4}}}});
        let r = map_output(&req, &p, &out, "jev-latest".into(), None).unwrap();
        match r.answers.get("c").unwrap() {
            JevAnswer::Choice { choice, .. } => assert_eq!(choice, "b"),
            _ => panic!("expected choice answer"),
        }
    }

    #[test]
    fn normalizes_relative_weights_and_maps_score() {
        let req: JevRequest = serde_json::from_value(json!({
            "state":"x",
            "questions":{"s":{"type":"score","criteria":["low","high"]}}
        }))
        .unwrap();
        let p = prepare_decision(&req).unwrap();
        let out = json!({"answers":{"q0":{"probabilities":{"l0":2.0,"l1":8.0}}}});
        let r = map_output(&req, &p, &out, "jev-latest".into(), None).unwrap();
        match r.answers.get("s").unwrap() {
            JevAnswer::Score { score, .. } => assert!((*score - 0.8).abs() < 1e-9),
            _ => panic!(),
        }
    }
}
