use crate::error::GatewayError;
use crate::model::*;
use hyper::StatusCode;
use serde_json::Value;

const MAX_NAME_CHARS: usize = 128;
const MAX_DOMAIN_CHARS: usize = 64;
const MAX_LOCALE_CHARS: usize = 35;
const MAX_TAG_CHARS: usize = 48;
const MAX_TERM_CHARS: usize = 64;
const MAX_DEFINITION_CHARS: usize = 256;
const MAX_STATEMENT_CHARS: usize = 256;
const MAX_FACT_NAME_CHARS: usize = 64;
const MAX_FACT_VALUE_CHARS: usize = 256;
const MAX_RULE_SIDE_CHARS: usize = 160;
const MAX_TERMS: usize = 32;
const MAX_CONSTRAINTS: usize = 16;
const MAX_FACTS: usize = 16;
const MAX_RULES: usize = 16;
const MAX_TAGS: usize = 16;

/// Positive/negative key pairs a noul `criteria` object may use. Keys match
/// case-insensitively; both keys must come from the same row (P7).
pub const CRITERIA_PAIRS: [(&str, &str); 8] = [
    ("true", "false"),
    ("yes", "no"),
    ("agree", "disagree"),
    ("accept", "reject"),
    ("approve", "deny"),
    ("allow", "block"),
    ("pass", "fail"),
    ("positive", "negative"),
];

const PAIR_KEYS_MESSAGE: &str = "criteria keys must be one positive/negative pair: true/false, \
yes/no, agree/disagree, accept/reject, approve/deny, allow/block, pass/fail, positive/negative";
const CRITERIA_TYPE_MESSAGE: &str =
    "noul criteria must be a string or a positive/negative pair object";

/// Counts text units in every object key and string value of `value`, at any
/// depth. Han, Hiragana, Katakana and Thai characters count one unit each;
/// other text counts runs of word characters (like the regex `\w+`).
/// Numbers, booleans and null count nothing.
pub fn text_units(value: &Value) -> usize {
    match value {
        Value::String(s) => str_units(s),
        Value::Array(xs) => xs.iter().map(text_units).sum(),
        Value::Object(xs) => xs.iter().map(|(k, v)| str_units(k) + text_units(v)).sum(),
        _ => 0,
    }
}

fn str_units(s: &str) -> usize {
    let mut units = 0;
    let mut in_word = false;
    for c in s.chars() {
        if is_per_char_script(c) {
            units += 1;
            in_word = false;
        } else if c.is_alphanumeric() || c == '_' {
            if !in_word {
                units += 1;
                in_word = true;
            }
        } else {
            in_word = false;
        }
    }
    units
}

/// Scripts written without spaces between words: each character counts.
fn is_per_char_script(c: char) -> bool {
    matches!(c,
        '\u{3400}'..='\u{4DBF}'
        | '\u{4E00}'..='\u{9FFF}'
        | '\u{F900}'..='\u{FAFF}'
        | '\u{20000}'..='\u{2A6DF}'
        | '\u{3040}'..='\u{309F}'
        | '\u{30A0}'..='\u{30FF}'
        | '\u{31F0}'..='\u{31FF}'
        | '\u{FF66}'..='\u{FF9F}'
        | '\u{0E00}'..='\u{0E7F}')
}

/// The `(positive, negative)` descriptions of a noul criteria object that has
/// exactly one key pair from [`CRITERIA_PAIRS`], else `None`.
pub fn criteria_pair(value: &Value) -> Option<(&Value, &Value)> {
    let Value::Object(map) = value else {
        return None;
    };
    if map.len() != 2 {
        return None;
    }
    let find = |name: &str| {
        map.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v)
    };
    CRITERIA_PAIRS
        .iter()
        .find_map(|(pos, neg)| Some((find(pos)?, find(neg)?)))
}

pub fn validate_request(
    req: &JevRequest,
    max_questions: usize,
    max_choice_options: usize,
    max_score_levels: usize,
    max_depth: usize,
    min_words: usize,
) -> Result<(), GatewayError> {
    if let Some(model) = &req.model {
        bounded_nonblank("model", model, 128)?;
        if model.chars().any(char::is_control) {
            return invalid("model contains a control character");
        }
    }
    if req.questions.is_empty() || req.questions.len() > max_questions {
        return invalid(format!(
            "questions must contain 1..={max_questions} entries"
        ));
    }
    if json_depth(&req.state, 0) > max_depth {
        return invalid(format!("state exceeds JSON nesting depth {max_depth}"));
    }
    validate_state(&req.state, min_words)?;

    for (name, q) in &req.questions {
        bounded_nonblank("question name", name, MAX_NAME_CHARS)?;
        match q {
            JevQuestion::Noul {
                instructions,
                criteria,
            } => {
                validate_optional_json(instructions.as_ref(), max_depth)?;
                validate_optional_json(criteria.as_ref(), max_depth)?;
                validate_noul(name, instructions.as_ref(), criteria.as_ref(), min_words)?;
            }
            JevQuestion::Choice {
                instructions,
                criteria,
            } => {
                validate_optional_json(instructions.as_ref(), max_depth)?;
                if criteria.is_empty() || criteria.len() > max_choice_options {
                    return invalid(format!(
                        "choice '{name}' criteria must contain 1..={max_choice_options} options"
                    ));
                }
                for (key, value) in criteria {
                    bounded_nonblank("choice option name", key, MAX_NAME_CHARS)?;
                    validate_json(value, max_depth)?;
                }
                if !meaningful(instructions.as_ref(), min_words)
                    && !criteria.iter().all(|(_, v)| text_units(v) >= min_words)
                {
                    return invalid(format!(
                        "question '{name}': choice needs instructions (at least {min_words} words) \
                         or a description of at least {min_words} words for every option"
                    ));
                }
            }
            JevQuestion::Score {
                instructions,
                criteria,
            } => {
                validate_optional_json(instructions.as_ref(), max_depth)?;
                // One level is allowed, as TypeSafe does (P5).
                if criteria.is_empty() || criteria.len() > max_score_levels {
                    return invalid(format!(
                        "score '{name}' criteria must contain 1..={max_score_levels} levels"
                    ));
                }
                for value in criteria {
                    validate_json(value, max_depth)?;
                }
                if !meaningful(instructions.as_ref(), min_words)
                    && !criteria.iter().all(|v| text_units(v) >= min_words)
                {
                    return invalid(format!(
                        "question '{name}': score needs instructions (at least {min_words} words) \
                         or a description of at least {min_words} words for every level"
                    ));
                }
            }
        }
    }

    if let Some(u) = &req.user_instructions {
        validate_user_instructions(u)?;
    }
    Ok(())
}

fn meaningful(value: Option<&Value>, min_words: usize) -> bool {
    value.is_some_and(|v| text_units(v) >= min_words)
}

/// `state` must carry text: not a bare scalar, not an array of only scalars,
/// and at least `min_words` units (P7).
fn validate_state(state: &Value, min_words: usize) -> Result<(), GatewayError> {
    fn scalars_only(v: &Value) -> bool {
        match v {
            Value::Array(xs) => xs.iter().all(scalars_only),
            Value::Null | Value::Bool(_) | Value::Number(_) => true,
            Value::String(_) | Value::Object(_) => false,
        }
    }
    match state {
        Value::Null | Value::Bool(_) | Value::Number(_) => {
            invalid("state must be a JSON object, array or string, not null, a number or a boolean")
        }
        Value::Array(_) if scalars_only(state) => {
            invalid("state must not be an array of only numbers, booleans or nulls")
        }
        _ if text_units(state) < min_words => invalid(format!(
            "state must contain at least {min_words} words of text in its keys or string values"
        )),
        _ => Ok(()),
    }
}

/// Noul: meaningful `instructions`, or a positive/negative `criteria` pair
/// whose descriptions are both meaningful. With instructions, `criteria` may
/// be absent, a string, or a pair with any descriptions (P7).
fn validate_noul(
    name: &str,
    instructions: Option<&Value>,
    criteria: Option<&Value>,
    min_words: usize,
) -> Result<(), GatewayError> {
    let pair = match criteria {
        None | Some(Value::Null) | Some(Value::String(_)) => None,
        Some(v @ Value::Object(_)) => match criteria_pair(v) {
            Some(pair) => Some(pair),
            None => return invalid(format!("question '{name}': {PAIR_KEYS_MESSAGE}")),
        },
        Some(_) => return invalid(format!("question '{name}': {CRITERIA_TYPE_MESSAGE}")),
    };
    if meaningful(instructions, min_words) {
        return Ok(());
    }
    if let Some((pos, neg)) = pair {
        if text_units(pos) >= min_words && text_units(neg) >= min_words {
            return Ok(());
        }
    }
    if matches!(criteria, Some(Value::String(_))) {
        return invalid(format!(
            "question '{name}': string criteria need instructions (at least {min_words} words); \
             write the question in instructions or use a positive/negative criteria pair (e.g. agree/disagree)"
        ));
    }
    invalid(format!(
        "question '{name}': noul needs instructions (at least {min_words} words) or a \
         positive/negative criteria pair (e.g. agree/disagree) with both descriptions"
    ))
}

pub fn validate_user_instructions(u: &UserInstructions) -> Result<(), GatewayError> {
    bounded_identifier("user_instructions.domain", &u.domain, MAX_DOMAIN_CHARS)?;
    if let Some(locale) = &u.locale {
        bounded_identifier("user_instructions.locale", locale, MAX_LOCALE_CHARS)?;
    }
    bounded_vec("terminology", u.terminology.len(), MAX_TERMS)?;
    bounded_vec("constraints", u.constraints.len(), MAX_CONSTRAINTS)?;
    bounded_vec("reference_facts", u.reference_facts.len(), MAX_FACTS)?;
    bounded_vec("decision_rules", u.decision_rules.len(), MAX_RULES)?;
    bounded_vec("tags", u.tags.len(), MAX_TAGS)?;

    for x in &u.terminology {
        bounded_nonblank("terminology.term", &x.term, MAX_TERM_CHARS)?;
        bounded_nonblank(
            "terminology.definition",
            &x.definition,
            MAX_DEFINITION_CHARS,
        )?;
    }
    for x in &u.constraints {
        bounded_nonblank("constraints.statement", &x.statement, MAX_STATEMENT_CHARS)?;
    }
    for x in &u.reference_facts {
        bounded_nonblank("reference_facts.name", &x.name, MAX_FACT_NAME_CHARS)?;
        bounded_nonblank("reference_facts.value", &x.value, MAX_FACT_VALUE_CHARS)?;
    }
    for x in &u.decision_rules {
        if x.priority > 100 {
            return invalid("decision_rules.priority must be 0..=100");
        }
        bounded_nonblank("decision_rules.when", &x.when, MAX_RULE_SIDE_CHARS)?;
        bounded_nonblank("decision_rules.then", &x.then, MAX_RULE_SIDE_CHARS)?;
    }
    for tag in &u.tags {
        bounded_identifier("tags[]", tag, MAX_TAG_CHARS)?;
    }
    Ok(())
}

fn validate_optional_json(value: Option<&Value>, max_depth: usize) -> Result<(), GatewayError> {
    if let Some(v) = value {
        validate_json(v, max_depth)?;
    }
    Ok(())
}

fn validate_json(value: &Value, max_depth: usize) -> Result<(), GatewayError> {
    if json_depth(value, 0) > max_depth {
        return invalid(format!("JSON value exceeds nesting depth {max_depth}"));
    }
    Ok(())
}

fn json_depth(value: &Value, depth: usize) -> usize {
    match value {
        Value::Array(xs) => xs
            .iter()
            .map(|x| json_depth(x, depth + 1))
            .max()
            .unwrap_or(depth + 1),
        Value::Object(xs) => xs
            .values()
            .map(|x| json_depth(x, depth + 1))
            .max()
            .unwrap_or(depth + 1),
        _ => depth,
    }
}

fn bounded_vec(name: &str, actual: usize, max: usize) -> Result<(), GatewayError> {
    if actual > max {
        return invalid(format!("user_instructions.{name} exceeds {max} entries"));
    }
    Ok(())
}

fn bounded_identifier(name: &str, s: &str, max: usize) -> Result<(), GatewayError> {
    bounded_nonblank(name, s, max)?;
    if !s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
    {
        return invalid(format!(
            "{name} may contain only ASCII letters, digits, '-', '_', '.', ':'"
        ));
    }
    Ok(())
}

fn bounded_nonblank(name: &str, s: &str, max: usize) -> Result<(), GatewayError> {
    let len = s.chars().count();
    if s.trim().is_empty() || len > max {
        return invalid(format!(
            "{name} must be non-blank and at most {max} characters"
        ));
    }
    Ok(())
}

fn invalid<T>(message: impl Into<String>) -> Result<T, GatewayError> {
    Err(GatewayError::local(
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_request",
        message,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const STATE: &str = "Payouts have failed for three days.";
    const ASK: &str = "Is the payout system down?";

    fn request(state: Value, questions: Value) -> JevRequest {
        serde_json::from_value(json!({"state": state, "questions": questions})).unwrap()
    }

    fn check(req: &JevRequest) -> Result<(), GatewayError> {
        validate_request(req, 32, 255, 10, 32, 3)
    }

    /// The 422 message for one question `q` against a meaningful state.
    fn question_error(q: Value) -> Option<String> {
        let req = request(json!(STATE), json!({ "q": q }));
        check(&req).err().map(|e| {
            assert_eq!(e.status, StatusCode::UNPROCESSABLE_ENTITY);
            assert_eq!(e.code, "invalid_request");
            e.message
        })
    }

    fn state_error(state: Value) -> Option<String> {
        let req = request(state, json!({"q": {"type": "noul", "instructions": ASK}}));
        check(&req).err().map(|e| e.message)
    }

    #[test]
    fn text_units_count_words_and_per_character_scripts() {
        let units = |v: Value| text_units(&v);
        assert_eq!(units(json!("Is the service down")), 4);
        assert_eq!(units(json!("down?!  yes, really... (ok)")), 4);
        assert_eq!(units(json!("snake_case_name and-dash")), 3);
        assert_eq!(units(json!("生产环境")), 4);
        assert_eq!(units(json!("宕机")), 2);
        assert_eq!(units(json!("ระบบล่ม")), 7);
        assert_eq!(units(json!("カタカナ ひらがな")), 8);
        assert_eq!(units(json!("서비스가 중단되었습니다")), 2);
        assert_eq!(units(json!("prod 宕机 now")), 4);
        assert_eq!(
            units(json!({"error_rate": "high", "notes": ["very", {"k": "v"}]})),
            6
        );
        assert_eq!(units(json!([1, 2.5, true, null, {"n": 3}])), 1);
        for empty in [
            json!(null),
            json!(""),
            json!("  \t"),
            json!({}),
            json!([]),
            json!(42),
        ] {
            assert_eq!(units(empty), 0);
        }
    }

    #[test]
    fn accepts_bounded_domain_context() {
        let req: JevRequest = serde_json::from_value(json!({
            "state": {"ticket": STATE},
            "user_instructions": {
                "domain": "support",
                "objective": "routing",
                "risk_posture": "balanced",
                "uncertainty_policy": "calibrated",
                "tags": ["payments"]
            },
            "questions": {"q": {"type":"noul", "instructions": ASK}}
        }))
        .unwrap();
        assert!(check(&req).is_ok());
    }

    #[test]
    fn rejects_oversized_domain_term_definition() {
        let req: JevRequest = serde_json::from_value(json!({
            "state": STATE,
            "user_instructions": {
                "domain": "support",
                "objective": "routing",
                "terminology": [{"term":"x", "definition": "a".repeat(257)}]
            },
            "questions": {"q": {"type":"noul", "instructions": ASK}}
        }))
        .unwrap();
        assert!(check(&req).is_err());
    }

    #[test]
    fn rejects_unknown_user_instruction_attribute() {
        let value = json!({
            "state": STATE,
            "user_instructions": {"domain":"support", "objective":"routing", "freeform":"no"},
            "questions": {"q": {"type":"noul"}}
        });
        assert!(serde_json::from_value::<JevRequest>(value).is_err());
    }

    #[test]
    fn score_levels_are_one_to_the_maximum() {
        let score = |n: usize| -> JevRequest {
            let levels: Vec<String> = (0..n).map(|i| format!("level {i}")).collect();
            request(
                json!(STATE),
                json!({"s": {"type": "score", "instructions": "Rate the payout urgency.", "criteria": levels}}),
            )
        };
        assert!(check(&score(0)).is_err());
        assert!(check(&score(1)).is_ok());
        assert!(check(&score(10)).is_ok());
        assert!(check(&score(11)).is_err());
    }

    #[test]
    fn noul_with_instructions_allows_absent_string_or_any_pair_criteria() {
        for criteria in [
            json!(null),
            json!("context only"),
            json!(""),
            json!({"true": "", "false": ""}),
            json!({"Agree": null, "DISAGREE": "x"}),
        ] {
            let q = json!({"type": "noul", "instructions": ASK, "criteria": criteria});
            assert_eq!(question_error(q), None, "criteria {criteria}");
        }
        assert_eq!(
            question_error(json!({"type": "noul", "instructions": ASK})),
            None
        );
    }

    #[test]
    fn noul_without_instructions_accepts_every_pair_row_case_insensitively() {
        let pos = "The payout system is down now.";
        let neg = "Payouts are working as normal.";
        for (p, n) in CRITERIA_PAIRS {
            for (p, n) in [
                (p.to_owned(), n.to_owned()),
                (p.to_uppercase(), n.to_uppercase()),
            ] {
                let q = json!({"type": "noul", "criteria": {p.clone(): pos, n.clone(): neg}});
                assert_eq!(question_error(q), None, "pair {p}/{n}");
            }
        }
        let q = json!({"type": "noul", "instructions": "", "criteria": {"Yes": pos, "nO": neg}});
        assert_eq!(question_error(q), None);
    }

    #[test]
    fn noul_rejects_criteria_that_are_not_one_pair() {
        let pair_message = "question 'q': criteria keys must be one positive/negative pair: \
            true/false, yes/no, agree/disagree, accept/reject, approve/deny, allow/block, \
            pass/fail, positive/negative";
        let d = "a long enough description";
        for criteria in [
            json!({"agree": d, "no": d}),
            json!({"true": d, "false": d, "maybe": d}),
            json!({"true": d}),
            json!({"up": d, "down": d}),
            json!({"Yes": d, "yes": d}),
            json!({}),
        ] {
            for instructions in [json!(ASK), json!(null)] {
                let q = json!({"type": "noul", "instructions": instructions, "criteria": criteria});
                assert_eq!(
                    question_error(q).as_deref(),
                    Some(pair_message),
                    "{criteria}"
                );
            }
        }
        for criteria in [json!(["yes", "no"]), json!(1), json!(true)] {
            let q = json!({"type": "noul", "instructions": ASK, "criteria": criteria});
            assert_eq!(
                question_error(q).as_deref(),
                Some("question 'q': noul criteria must be a string or a positive/negative pair object")
            );
        }
    }

    #[test]
    fn noul_without_instructions_needs_two_meaningful_descriptions() {
        let noul_message = "question 'q': noul needs instructions (at least 3 words) or a \
            positive/negative criteria pair (e.g. agree/disagree) with both descriptions";
        for q in [
            json!({"type": "noul"}),
            json!({"type": "noul", "instructions": "Down?", "criteria": null}),
            json!({"type": "noul", "criteria": {"agree": "The system is down.", "disagree": "up"}}),
            json!({"type": "noul", "criteria": {"true": "", "false": "Payouts work normally."}}),
        ] {
            assert_eq!(
                question_error(q.clone()).as_deref(),
                Some(noul_message),
                "{q}"
            );
        }
        let q = json!({"type": "noul", "criteria": "Is the payout system down?"});
        assert_eq!(
            question_error(q).as_deref(),
            Some(
                "question 'q': string criteria need instructions (at least 3 words); write the \
                 question in instructions or use a positive/negative criteria pair (e.g. agree/disagree)"
            )
        );
    }

    #[test]
    fn choice_needs_instructions_or_every_description() {
        let described = json!({
            "billing": "Billing or invoice problem.",
            "outage": "Production outage in progress."
        });
        let q = json!({"type": "choice", "criteria": described});
        assert_eq!(question_error(q), None);
        let q = json!({"type": "choice", "instructions": "Pick the reviewing team.",
                       "criteria": {"billing": "", "outage": null, "other": 3}});
        assert_eq!(question_error(q), None);
        let q = json!({"type": "choice", "criteria": {"billing": "Billing or invoice problem.", "outage": "Outage"}});
        assert_eq!(
            question_error(q).as_deref(),
            Some(
                "question 'q': choice needs instructions (at least 3 words) or a description of \
                 at least 3 words for every option"
            )
        );
    }

    #[test]
    fn score_needs_instructions_or_every_description() {
        let q = json!({"type": "score", "criteria": ["Routine, can wait.", "Needs attention today.", "Immediate human attention"]});
        assert_eq!(question_error(q), None);
        let q = json!({"type": "score", "instructions": "Rate the urgency.", "criteria": ["", null, "high"]});
        assert_eq!(question_error(q), None);
        let q = json!({"type": "score", "criteria": ["low", "high"]});
        assert_eq!(
            question_error(q).as_deref(),
            Some(
                "question 'q': score needs instructions (at least 3 words) or a description of \
                 at least 3 words for every level"
            )
        );
    }

    #[test]
    fn state_needs_meaningful_text() {
        let scalar =
            "state must be a JSON object, array or string, not null, a number or a boolean";
        for state in [json!(null), json!(7), json!(1.5), json!(true)] {
            assert_eq!(state_error(state).as_deref(), Some(scalar));
        }
        let numbers = "state must not be an array of only numbers, booleans or nulls";
        for state in [
            json!([1, true, [2]]),
            json!([[1, 2], [true]]),
            json!([1, null, true]),
            json!([]),
        ] {
            assert_eq!(state_error(state).as_deref(), Some(numbers));
        }
        let words = "state must contain at least 3 words of text in its keys or string values";
        for state in [
            json!(""),
            json!("   "),
            json!({}),
            json!({"temp": 38.5, "fever": true}),
            json!(["a", 1]),
        ] {
            assert_eq!(
                state_error(state.clone()).as_deref(),
                Some(words),
                "{state}"
            );
        }
        for state in [
            json!(STATE),
            json!({"temp": 38.5, "fever": true, "cough": "dry"}),
            json!(["patient", "has", "fever"]),
            json!("生产环境宕机"),
        ] {
            assert_eq!(state_error(state.clone()), None, "{state}");
        }
    }

    #[test]
    fn the_published_example_request_passes() {
        let text = include_str!("../../../examples/request.json");
        let req: JevRequest = serde_json::from_str(text).unwrap();
        assert!(check(&req).is_ok());
    }

    #[test]
    fn the_word_threshold_is_configurable() {
        let req = request(
            json!("down"),
            json!({"q": {"type": "noul", "instructions": "Down?"}}),
        );
        assert!(validate_request(&req, 32, 255, 10, 32, 1).is_ok());
        assert!(validate_request(&req, 32, 255, 10, 32, 2).is_err());
    }
}
