use std::{collections::HashMap, sync::Mutex};

/// Default `oversize` body size: above the gateway's default 1 MiB limit.
pub const DEFAULT_OVERSIZE_BYTES: usize = 2 * 1024 * 1024;
/// Upper bound for `slow:<ms>`.
pub const MAX_SLOW_MS: u64 = 120_000;

/// One reply mode, chosen by `x-mock-scenario` or `MOCK_SCENARIO`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scenario {
    Ok,
    Refusal,
    MaxTokens,
    NoText,
    BadJson,
    Oversize(usize),
    NoUsage,
    /// Error reply with this status (400..=599).
    Http(u16),
    /// Sleep this many milliseconds (already capped), then `Ok`.
    Slow(u64),
    /// The first `n` requests carrying this exact value fail with 503.
    Flaky(u64),
    /// A successful reply whose output text is valid JSON without `answers`:
    /// always (`None`), or for the first `n` requests carrying this exact
    /// value, then `Ok` (`Some(n)`).
    WrongShape(Option<u64>),
}

impl Scenario {
    pub fn parse(value: &str) -> Result<Self, String> {
        let unknown = || format!("unknown mock scenario {value:?}");
        let scenario = match value {
            "ok" => Self::Ok,
            "refusal" => Self::Refusal,
            "max_tokens" => Self::MaxTokens,
            "no_text" => Self::NoText,
            "bad_json" => Self::BadJson,
            "oversize" => Self::Oversize(DEFAULT_OVERSIZE_BYTES),
            "no_usage" => Self::NoUsage,
            "wrong_shape" => Self::WrongShape(None),
            _ => {
                if let Some(bytes) = value.strip_prefix("oversize:") {
                    Self::Oversize(bytes.parse().map_err(|_| unknown())?)
                } else if let Some(status) = value.strip_prefix("http_") {
                    let status: u16 = status.parse().map_err(|_| unknown())?;
                    if !(400..=599).contains(&status) {
                        return Err(unknown());
                    }
                    Self::Http(status)
                } else if let Some(ms) = value.strip_prefix("slow:") {
                    let ms: u64 = ms.parse().map_err(|_| unknown())?;
                    Self::Slow(ms.min(MAX_SLOW_MS))
                } else if let Some(n) = value.strip_prefix("flaky:") {
                    Self::Flaky(n.parse().map_err(|_| unknown())?)
                } else if let Some(n) = value.strip_prefix("wrong_shape:") {
                    Self::WrongShape(Some(n.parse().map_err(|_| unknown())?))
                } else {
                    return Err(unknown());
                }
            }
        };
        Ok(scenario)
    }
}

/// Per-instance `flaky` and `wrong_shape:<n>` counters keyed by the exact
/// scenario value.
#[derive(Default)]
pub struct FlakyCounters(Mutex<HashMap<String, u64>>);

impl FlakyCounters {
    /// Counts one request for `value`; true while it is within the first `n`.
    pub fn should_fail(&self, value: &str, n: u64) -> bool {
        let mut counts = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let count = counts.entry(value.to_owned()).or_insert(0);
        *count += 1;
        *count <= n
    }

    pub fn clear(&self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_named_and_parameterized_scenarios() {
        assert_eq!(Scenario::parse("ok"), Ok(Scenario::Ok));
        assert_eq!(Scenario::parse("no_usage"), Ok(Scenario::NoUsage));
        assert_eq!(Scenario::parse("http_401"), Ok(Scenario::Http(401)));
        assert_eq!(Scenario::parse("http_529"), Ok(Scenario::Http(529)));
        assert_eq!(
            Scenario::parse("oversize"),
            Ok(Scenario::Oversize(DEFAULT_OVERSIZE_BYTES))
        );
        assert_eq!(Scenario::parse("oversize:10"), Ok(Scenario::Oversize(10)));
        assert_eq!(Scenario::parse("slow:1000"), Ok(Scenario::Slow(1000)));
        assert_eq!(
            Scenario::parse("slow:999999"),
            Ok(Scenario::Slow(MAX_SLOW_MS))
        );
        assert_eq!(Scenario::parse("flaky:2"), Ok(Scenario::Flaky(2)));
        assert_eq!(
            Scenario::parse("wrong_shape"),
            Ok(Scenario::WrongShape(None))
        );
        assert_eq!(
            Scenario::parse("wrong_shape:2"),
            Ok(Scenario::WrongShape(Some(2)))
        );
    }

    #[test]
    fn rejects_unknown_scenarios() {
        for value in [
            "http_600",
            "http_399",
            "http_abc",
            "slow:",
            "flaky:x",
            "wrong_shape:x",
            "nope",
            "",
        ] {
            let err = Scenario::parse(value).unwrap_err();
            assert!(err.contains("unknown mock scenario"), "{value}: {err}");
        }
    }

    #[test]
    fn flaky_counter_fails_first_n_per_value() {
        let counters = FlakyCounters::default();
        assert!(counters.should_fail("flaky:2", 2));
        assert!(counters.should_fail("flaky:1", 1));
        assert!(counters.should_fail("flaky:2", 2));
        assert!(!counters.should_fail("flaky:2", 2));
        assert!(!counters.should_fail("flaky:1", 1));
        counters.clear();
        assert!(counters.should_fail("flaky:2", 2));
    }
}
