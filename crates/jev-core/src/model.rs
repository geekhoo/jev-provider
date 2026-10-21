use serde::de::{MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use std::fmt;
use std::marker::PhantomData;

/// Deserialize from the request bytes, not from a `serde_json::Value`: its
/// map is sorted, and answers follow request order (P4).
#[derive(Debug, Clone, Deserialize)]
pub struct JevRequest {
    #[serde(default)]
    pub model: Option<String>,
    pub state: Value,
    /// In request order.
    pub questions: OrderedMap<JevQuestion>,
    #[serde(default)]
    pub user_instructions: Option<UserInstructions>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum JevQuestion {
    Noul {
        #[serde(default)]
        instructions: Option<Value>,
        #[serde(default)]
        criteria: Option<Value>,
    },
    Choice {
        #[serde(default)]
        instructions: Option<Value>,
        /// In request order.
        criteria: OrderedMap<Value>,
    },
    Score {
        #[serde(default)]
        instructions: Option<Value>,
        criteria: Vec<Value>,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UserInstructions {
    pub domain: String,
    #[serde(default)]
    pub locale: Option<String>,
    pub objective: DecisionObjective,
    #[serde(default)]
    pub risk_posture: RiskPosture,
    #[serde(default)]
    pub uncertainty_policy: UncertaintyPolicy,
    #[serde(default)]
    pub terminology: Vec<TermDefinition>,
    #[serde(default)]
    pub constraints: Vec<DomainConstraint>,
    #[serde(default)]
    pub reference_facts: Vec<ReferenceFact>,
    #[serde(default)]
    pub decision_rules: Vec<DecisionRule>,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionObjective {
    Classification,
    Routing,
    Gating,
    Scoring,
    Ranking,
    Compliance,
    QualityControl,
    RiskAssessment,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskPosture {
    Conservative,
    #[default]
    Balanced,
    Permissive,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UncertaintyPolicy {
    #[default]
    Calibrated,
    Conservative,
    AbstainBiased,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TermDefinition {
    pub term: String,
    pub definition: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DomainConstraint {
    pub kind: ConstraintKind,
    pub statement: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConstraintKind {
    Must,
    MustNot,
    Prefer,
    Avoid,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceFact {
    pub name: String,
    pub value: String,
    pub authority: FactAuthority,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FactAuthority {
    Contract,
    Policy,
    Taxonomy,
    Product,
    Regulatory,
    DomainConvention,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionRule {
    pub priority: u8,
    pub when: String,
    pub then: String,
}

#[derive(Debug, Serialize)]
pub struct JevResponse {
    pub model: String,
    /// Keyed by question name, in request order.
    pub answers: OrderedMap<JevAnswer>,
    pub usage: Usage,
}

/// Fields serialize in declaration order after `type`, matching the official
/// output (P4).
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum JevAnswer {
    Noul {
        noul: f64,
    },
    Choice {
        choice: String,
        confidence: f64,
        /// Option names in request order.
        probabilities: OrderedMap<f64>,
    },
    Score {
        score: f64,
        confidence: f64,
        /// Levels in numeric order `"0"`..`"n-1"`.
        legend: OrderedMap<Value>,
        /// Levels in numeric order `"0"`..`"n-1"`.
        probabilities: OrderedMap<f64>,
    },
}

/// A JSON object that keeps its entries in insertion (or document) order.
/// Keys are unique: by construction, or checked when deserialized. Lookups
/// are linear; the maps here hold at most a few hundred entries.
#[derive(Debug, Clone, PartialEq)]
pub struct OrderedMap<V>(pub Vec<(String, V)>);

impl<V> OrderedMap<V> {
    pub fn get(&self, key: &str) -> Option<&V> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &V)> {
        self.0.iter().map(|(k, v)| (k, v))
    }

    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.0.iter().map(|(k, _)| k)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<'a, V> IntoIterator for &'a OrderedMap<V> {
    type Item = (&'a String, &'a V);
    type IntoIter =
        std::iter::Map<std::slice::Iter<'a, (String, V)>, fn(&(String, V)) -> (&String, &V)>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter().map(|(k, v)| (k, v))
    }
}

impl<V> std::ops::Index<&str> for OrderedMap<V> {
    type Output = V;

    fn index(&self, key: &str) -> &V {
        self.get(key).expect("key present in OrderedMap")
    }
}

impl<V: Serialize> Serialize for OrderedMap<V> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (k, v) in &self.0 {
            map.serialize_entry(k, v)?;
        }
        map.end()
    }
}

impl<'de, V: Deserialize<'de>> Deserialize<'de> for OrderedMap<V> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct OrderedVisitor<V>(PhantomData<V>);

        impl<'de, V: Deserialize<'de>> Visitor<'de> for OrderedVisitor<V> {
            type Value = OrderedMap<V>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a JSON object without duplicate keys")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
                let mut entries: Vec<(String, V)> = Vec::new();
                while let Some((key, value)) = access.next_entry::<String, V>()? {
                    if entries.iter().any(|(k, _)| *k == key) {
                        return Err(serde::de::Error::custom(format!("duplicate key {key:?}")));
                    }
                    entries.push((key, value));
                }
                Ok(OrderedMap(entries))
            }
        }

        deserializer.deserialize_map(OrderedVisitor(PhantomData))
    }
}

/// Token counts; a count the upstream did not report serializes as `null`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct PreparedQuestion {
    pub request_id: String,
    pub internal_id: String,
    pub kind: PreparedKind,
    pub options: Vec<PreparedOption>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreparedKind {
    Noul,
    Choice,
    Score,
}

#[derive(Debug, Clone)]
pub struct PreparedOption {
    pub request_key: String,
    pub internal_key: String,
}

#[derive(Debug)]
pub struct PreparedDecision {
    pub questions: Vec<PreparedQuestion>,
    pub schema: Value,
    pub input: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordered_map_keeps_document_order_and_rejects_duplicates() {
        let text = r#"{"b":1,"a":2,"10":3,"9":4}"#;
        let m: OrderedMap<u8> = serde_json::from_str(text).unwrap();
        assert_eq!(m.keys().collect::<Vec<_>>(), ["b", "a", "10", "9"]);
        assert_eq!(serde_json::to_string(&m).unwrap(), text);
        assert!(serde_json::from_str::<OrderedMap<u8>>(r#"{"a":1,"b":2,"a":3}"#).is_err());
    }
}
