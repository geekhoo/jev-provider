//! Deterministic schema-conforming answers (§5.5).

use serde_json::{json, Map, Value};

/// The marker `jev-core` puts before the output schema in prompt-mode text.
const SCHEMA_MARKER: &str =
    "OUTPUT JSON SCHEMA (return exactly one JSON object matching this schema):\n";

/// A deterministic value that conforms to `schema`.
pub fn conforming(schema: &Value) -> Value {
    if let Some(first) = schema["enum"].as_array().and_then(|values| values.first()) {
        return first.clone();
    }
    if let Some(value) = schema.get("const") {
        return value.clone();
    }
    for key in ["anyOf", "oneOf"] {
        if let Some(branch) = schema[key].as_array().and_then(|branches| branches.first()) {
            return conforming(branch);
        }
    }
    match schema["type"].as_str() {
        Some("object") => object(schema),
        Some("number") => json!(0.75),
        Some("integer") => json!(1),
        Some("string") => json!("mock"),
        Some("boolean") => json!(true),
        Some("array") => json!([]),
        _ => Value::Null,
    }
}

/// Weights for an all-`number` object with n ≥ 2 properties, otherwise a
/// recursion; both follow `required` order because `serde_json` maps sort
/// `o10` before `o2`.
fn object(schema: &Value) -> Value {
    let properties = schema["properties"].as_object();
    let keys: Vec<&str> = match schema["required"].as_array() {
        Some(required) => required.iter().filter_map(Value::as_str).collect(),
        None => properties.map_or_else(Vec::new, |p| p.keys().map(String::as_str).collect()),
    };
    let property = |key: &str| properties.and_then(|p| p.get(key)).unwrap_or(&Value::Null);
    let n = keys.len();
    let weights = n >= 2
        && properties.is_some_and(|p| p.len() == n)
        && keys.iter().all(|key| property(key)["type"] == "number");
    let total = (n * (n + 1) / 2) as f64;
    let mut out = Map::new();
    for (i, key) in keys.into_iter().enumerate() {
        let value = if weights {
            json!((i + 1) as f64 / total)
        } else {
            conforming(property(key))
        };
        out.insert(key.to_owned(), value);
    }
    Value::Object(out)
}

/// The JSON schema after the last output-schema marker in a prompt, if any.
pub fn schema_from_text(text: &str) -> Option<Value> {
    let (_, rest) = text.rsplit_once(SCHEMA_MARKER)?;
    let schema = serde_json::Deserializer::from_str(rest)
        .into_iter::<Value>()
        .next()?
        .ok()?;
    schema.is_object().then_some(schema)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn number() -> Value {
        json!({ "type": "number", "minimum": 0, "maximum": 1 })
    }

    fn probabilities(keys: &[String]) -> Value {
        let properties: Map<String, Value> = keys.iter().map(|k| (k.clone(), number())).collect();
        json!({
            "type": "object",
            "properties": {
                "probabilities": {
                    "type": "object",
                    "properties": properties,
                    "required": keys,
                    "additionalProperties": false,
                }
            },
            "required": ["probabilities"],
            "additionalProperties": false,
        })
    }

    fn keys(prefix: &str, n: usize) -> Vec<String> {
        (0..n).map(|i| format!("{prefix}{i}")).collect()
    }

    /// The schema `jev-core` generates for `examples/request.json`: questions
    /// sorted to `q0` = is_blocking (noul), `q1` = team (choice, options
    /// sorted to billing, general_support, incident_response), `q2` = urgency
    /// (score, three levels).
    fn example_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "answers": {
                    "type": "object",
                    "properties": {
                        "q0": {
                            "type": "object",
                            "properties": { "probability": number() },
                            "required": ["probability"],
                            "additionalProperties": false,
                        },
                        "q1": probabilities(&keys("o", 3)),
                        "q2": probabilities(&keys("l", 3)),
                    },
                    "required": ["q0", "q1", "q2"],
                    "additionalProperties": false,
                }
            },
            "required": ["answers"],
            "additionalProperties": false,
        })
    }

    #[test]
    fn example_request_weights() {
        let answers = &conforming(&example_schema())["answers"];
        assert_eq!(answers["q0"], json!({ "probability": 0.75 }));
        let sixth = |i: f64| json!(i / 6.0);
        for q in ["q1", "q2"] {
            let p = &answers[q]["probabilities"];
            let prefix = if q == "q1" { "o" } else { "l" };
            for i in 0..3 {
                assert_eq!(p[format!("{prefix}{i}")], sixth((i + 1) as f64), "{q}");
            }
        }
        // Jev side: billing 1/6, general_support 2/6, incident_response 3/6;
        // urgency score = 0·1/6 + 1·2/6 + 2·3/6 = 4/3.
        let p = &answers["q2"]["probabilities"];
        let score: f64 = (0..3)
            .map(|i| i as f64 * p[format!("l{i}")].as_f64().unwrap())
            .sum();
        assert!((score - 1.333333).abs() < 1e-6);
    }

    #[test]
    fn weights_follow_required_order_with_eleven_options() {
        let names = keys("o", 11);
        let schema = probabilities(&names);
        // `properties` iterates o0, o1, o10, o2, ...; `required` must win.
        let props = &schema["properties"]["probabilities"]["properties"];
        assert_eq!(props.as_object().unwrap().keys().nth(2).unwrap(), "o10");
        let p = &conforming(&schema)["probabilities"];
        let total = 66.0;
        for (i, name) in names.iter().enumerate() {
            assert_eq!(p[name], json!((i + 1) as f64 / total), "{name}");
        }
        assert_eq!(p["o10"], json!(11.0 / 66.0));
        assert!(p["o10"].as_f64() > p["o9"].as_f64());
        let sum: f64 = p
            .as_object()
            .unwrap()
            .values()
            .filter_map(Value::as_f64)
            .sum();
        assert!((sum - 1.0).abs() < 1e-9);
    }

    /// A one-level Score (P5) gets the plain number value; the gateway
    /// normalizes it to probability 1.
    #[test]
    fn single_level_probabilities() {
        assert_eq!(
            conforming(&probabilities(&keys("l", 1))),
            json!({ "probabilities": { "l0": 0.75 } })
        );
    }

    #[test]
    fn nested_objects_and_scalars() {
        let schema = json!({
            "type": "object",
            "properties": {
                "a": { "type": "object", "properties": {
                    "n": { "type": "number" },
                    "s": { "type": "string" },
                } },
                "b": { "type": "integer" },
                "c": { "type": "boolean" },
                "d": { "type": "array", "items": { "type": "string" } },
                "e": { "type": "null" },
                "f": { "enum": ["x", "y"] },
                "g": { "const": 3 },
                "h": { "anyOf": [{ "type": "string" }, { "type": "null" }] },
                "i": { "oneOf": [{ "type": "integer" }] },
            },
        });
        assert_eq!(
            conforming(&schema),
            json!({
                "a": { "n": 0.75, "s": "mock" },
                "b": 1,
                "c": true,
                "d": [],
                "e": null,
                "f": "x",
                "g": 3,
                "h": "mock",
                "i": 1,
            })
        );
        assert_eq!(conforming(&json!({ "type": "number" })), json!(0.75));
        // A mixed object is not a weight vector.
        let mixed = json!({
            "type": "object",
            "properties": { "x": { "type": "number" }, "y": { "type": "string" } },
            "required": ["x", "y"],
        });
        assert_eq!(conforming(&mixed), json!({ "x": 0.75, "y": "mock" }));
    }

    #[test]
    fn schema_from_text_with_and_without_marker() {
        let schema = example_schema();
        let text =
            format!("first {SCHEMA_MARKER}{{\"stale\":true}}\n\nINPUT\n\n{SCHEMA_MARKER}{schema}");
        assert_eq!(schema_from_text(&text), Some(schema));
        assert_eq!(schema_from_text("INPUT with no schema"), None);
        assert_eq!(schema_from_text(&format!("x{SCHEMA_MARKER}not json")), None);
        assert_eq!(schema_from_text(&format!("x{SCHEMA_MARKER}[1]")), None);
    }
}
