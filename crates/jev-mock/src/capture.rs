use hyper::HeaderMap;
use serde_json::{json, Map, Value};
use std::{collections::VecDeque, sync::Mutex};

/// Stored request bodies are truncated to this many bytes.
pub const MAX_STORED_BODY: usize = 64 * 1024;
const REDACTED: [&str; 3] = ["authorization", "x-api-key", "cookie"];

/// Bounded ring of captured requests, oldest first.
pub struct Capture {
    limit: usize,
    entries: Mutex<VecDeque<Value>>,
}

impl Capture {
    pub fn new(limit: usize) -> Self {
        Self {
            limit,
            entries: Mutex::new(VecDeque::new()),
        }
    }

    pub fn push(&self, entry: Value) {
        if self.limit == 0 {
            return;
        }
        let mut entries = self.lock();
        while entries.len() >= self.limit {
            entries.pop_front();
        }
        entries.push_back(entry);
    }

    /// The newest `limit` entries (all when `None`), newest last.
    pub fn recent(&self, limit: Option<usize>) -> Vec<Value> {
        let entries = self.lock();
        let skip = limit.map_or(0, |n| entries.len().saturating_sub(n));
        entries.iter().skip(skip).cloned().collect()
    }

    pub fn clear(&self) {
        self.lock().clear();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<Value>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Header names are already lower-case in `HeaderMap`; key values are
/// replaced with `"[redacted]"` and repeated headers are joined with ", ".
pub fn headers_json(headers: &HeaderMap) -> Value {
    let mut out = Map::new();
    for (name, value) in headers {
        let name = name.as_str();
        let value = if REDACTED.contains(&name) {
            "[redacted]".to_owned()
        } else {
            String::from_utf8_lossy(value.as_bytes()).into_owned()
        };
        match out.get_mut(name) {
            Some(Value::String(prev)) if !REDACTED.contains(&name) => {
                prev.push_str(", ");
                prev.push_str(&value);
            }
            Some(_) => {}
            None => {
                out.insert(name.to_owned(), Value::String(value));
            }
        }
    }
    Value::Object(out)
}

/// The body as JSON when it parses, else as a string; at most 64 KiB kept.
pub fn body_json(body: &[u8]) -> Value {
    if body.len() <= MAX_STORED_BODY {
        if let Ok(value) = serde_json::from_slice(body) {
            return value;
        }
    }
    let kept = &body[..body.len().min(MAX_STORED_BODY)];
    Value::String(String::from_utf8_lossy(kept).into_owned())
}

pub fn entry(
    seq: u64,
    method: &str,
    path: &str,
    scenario: Option<&str>,
    headers: &HeaderMap,
    body: Value,
    contract_errors: &[String],
) -> Value {
    json!({
        "seq": seq,
        "method": method,
        "path": path,
        "scenario": scenario,
        "headers": headers_json(headers),
        "body": body,
        "contract_errors": contract_errors,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::header::HeaderValue;

    #[test]
    fn redacts_key_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_static("Bearer sk-secret"),
        );
        headers.insert("x-api-key", HeaderValue::from_static("sk-ant-secret"));
        headers.insert("cookie", HeaderValue::from_static("session=secret"));
        headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        headers.append("x-multi", HeaderValue::from_static("a"));
        headers.append("x-multi", HeaderValue::from_static("b"));
        let json = headers_json(&headers);
        assert_eq!(json["authorization"], "[redacted]");
        assert_eq!(json["x-api-key"], "[redacted]");
        assert_eq!(json["cookie"], "[redacted]");
        assert_eq!(json["anthropic-version"], "2023-06-01");
        assert_eq!(json["x-multi"], "a, b");
        assert!(!json.to_string().contains("secret"));
    }

    #[test]
    fn ring_keeps_newest_entries_up_to_limit() {
        let capture = Capture::new(3);
        for seq in 1..=5 {
            capture.push(json!({ "seq": seq }));
        }
        let seqs: Vec<_> = capture
            .recent(None)
            .iter()
            .map(|e| e["seq"].clone())
            .collect();
        assert_eq!(seqs, [json!(3), json!(4), json!(5)]);
        assert_eq!(capture.recent(Some(1)), [json!({ "seq": 5 })]);
        capture.clear();
        assert!(capture.recent(None).is_empty());

        let disabled = Capture::new(0);
        disabled.push(json!({}));
        assert!(disabled.recent(None).is_empty());
    }

    #[test]
    fn body_is_json_or_truncated_string() {
        assert_eq!(body_json(br#"{"a":1}"#), json!({ "a": 1 }));
        assert_eq!(body_json(b"not json"), json!("not json"));
        let big = vec![b'x'; MAX_STORED_BODY + 10];
        assert_eq!(body_json(&big).as_str().unwrap().len(), MAX_STORED_BODY);
    }
}
