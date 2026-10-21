use crate::provider::ApiType;
use bytes::Bytes;
use std::{
    collections::HashMap,
    hash::{BuildHasher, RandomState},
    sync::Mutex,
    time::{Duration, Instant},
};

const SHARDS: usize = 64;

#[derive(Clone)]
pub struct CachedResponse {
    pub request_id: String,
    pub body: Bytes,
    pub upstream_request_id: Option<String>,
    pub provider: String,
    pub upstream_model: String,
    pub api_type: ApiType,
}

enum EntryState {
    InProgress,
    Success(CachedResponse),
    Failed,
}

/// Request body length and keyed hash. Holding a digest instead of the body
/// keeps the cache bound independent of `MAX_BODY_BYTES`.
#[derive(Clone, Copy, PartialEq, Eq)]
struct BodyDigest {
    len: usize,
    hash: u64,
}

struct Entry {
    body_digest: BodyDigest,
    created: Instant,
    state: EntryState,
}

pub enum BeginResult {
    Started,
    Replay(CachedResponse),
    InProgress,
    Failed,
    Conflict,
}

pub struct IdempotencyCache {
    shards: Vec<Mutex<HashMap<String, Entry>>>,
    ttl: Duration,
    max_entries_per_shard: usize,
    // Process-local random keys make body collisions unpredictable to callers.
    hasher: RandomState,
}

impl IdempotencyCache {
    pub fn new(ttl: Duration, max_entries: usize) -> Self {
        let per = max_entries.max(SHARDS).div_ceil(SHARDS);
        let shards = (0..SHARDS).map(|_| Mutex::new(HashMap::new())).collect();
        Self {
            shards,
            ttl,
            max_entries_per_shard: per,
            hasher: RandomState::new(),
        }
    }

    pub fn begin(&self, namespace_key: String, request_body: Bytes) -> BeginResult {
        let body_digest = BodyDigest {
            len: request_body.len(),
            hash: self.hasher.hash_one(&request_body[..]),
        };
        let idx = shard_index(&namespace_key);
        let mut map = self.shards[idx].lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        if let Some(entry) = map.get(&namespace_key) {
            if now.duration_since(entry.created) <= self.ttl {
                if entry.body_digest != body_digest {
                    return BeginResult::Conflict;
                }
                return match &entry.state {
                    EntryState::InProgress => BeginResult::InProgress,
                    EntryState::Success(cached) => BeginResult::Replay(cached.clone()),
                    EntryState::Failed => BeginResult::Failed,
                };
            }
        }
        map.remove(&namespace_key);
        if map.len() >= self.max_entries_per_shard {
            if let Some(oldest) = map
                .iter()
                .min_by_key(|(_, e)| e.created)
                .map(|(k, _)| k.clone())
            {
                map.remove(&oldest);
            }
        }
        map.insert(
            namespace_key,
            Entry {
                body_digest,
                created: now,
                state: EntryState::InProgress,
            },
        );
        BeginResult::Started
    }

    pub fn success(&self, namespace_key: &str, cached: CachedResponse) {
        let idx = shard_index(namespace_key);
        let mut map = self.shards[idx].lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = map.get_mut(namespace_key) {
            entry.state = EntryState::Success(cached);
        }
    }

    pub fn failed(&self, namespace_key: &str) {
        let idx = shard_index(namespace_key);
        let mut map = self.shards[idx].lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = map.get_mut(namespace_key) {
            entry.state = EntryState::Failed;
        }
    }
}

fn shard_index(key: &str) -> usize {
    // FNV-1a is used only for shard selection, not authentication/security.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in key.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    (h as usize) & (SHARDS - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_maximum_capacity_without_overflow() {
        let cache = IdempotencyCache::new(Duration::from_secs(60), usize::MAX);
        assert!(matches!(
            cache.begin("k".into(), Bytes::from_static(b"a")),
            BeginResult::Started
        ));
    }

    #[test]
    fn conflicts_on_same_key_different_body() {
        let cache = IdempotencyCache::new(Duration::from_secs(60), 64);
        assert!(matches!(
            cache.begin("k".into(), Bytes::from_static(b"a")),
            BeginResult::Started
        ));
        assert!(matches!(
            cache.begin("k".into(), Bytes::from_static(b"b")),
            BeginResult::Conflict
        ));
        // Different lengths conflict too, and the original entry is kept.
        assert!(matches!(
            cache.begin("k".into(), Bytes::from_static(b"ab")),
            BeginResult::Conflict
        ));
        assert!(matches!(
            cache.begin("k".into(), Bytes::from_static(b"a")),
            BeginResult::InProgress
        ));
    }

    #[test]
    fn reports_failure_for_same_body() {
        let cache = IdempotencyCache::new(Duration::from_secs(60), 64);
        let body = Bytes::from_static(b"a");
        assert!(matches!(
            cache.begin("k".into(), body.clone()),
            BeginResult::Started
        ));
        cache.failed("k");
        assert!(matches!(cache.begin("k".into(), body), BeginResult::Failed));
    }

    #[test]
    fn replays_success() {
        let cache = IdempotencyCache::new(Duration::from_secs(60), 64);
        let body = Bytes::from_static(b"a");
        assert!(matches!(
            cache.begin("k".into(), body.clone()),
            BeginResult::Started
        ));
        cache.success(
            "k",
            CachedResponse {
                request_id: "r1".into(),
                body: Bytes::from_static(b"{}"),
                upstream_request_id: None,
                provider: "p".into(),
                upstream_model: "m".into(),
                api_type: ApiType::ResponsesApi,
            },
        );
        // A fresh buffer with equal bytes matches the stored digest.
        let BeginResult::Replay(cached) = cache.begin("k".into(), Bytes::from(b"a".to_vec()))
        else {
            panic!("expected a replay");
        };
        assert_eq!(cached.request_id, "r1");
        assert_eq!(cached.body, Bytes::from_static(b"{}"));
        assert!(matches!(
            cache.begin("k".into(), body),
            BeginResult::Replay(_)
        ));
    }
}
