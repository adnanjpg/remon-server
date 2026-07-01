//! Process-local positive cache over the `sessions` table.
//!
//! The auth middleware confirms every request's jti against `sessions` to
//! enforce revocation. That lookup is a µs-scale PK probe, but it borrows a
//! connection from the same pool as metrics-history scans and collector
//! writes — under load an auth check queues behind whatever slow query holds
//! the last free connection. Caching confirmed jtis keeps the hot path
//! in-memory and decouples request latency from DB contention.
//!
//! Correctness contract:
//! - Only *positive* lookups are cached. A jti minted by a concurrent
//!   refresh must authenticate on its first use, so "not found" is never
//!   remembered.
//! - Revocation paths evict explicitly: logout evicts its own jti; refresh
//!   rotation and device revocation clear the whole map (device deletes
//!   cascade to `sessions` inside SQLite, so there is no per-jti signal to
//!   hook — and the map is a handful of entries, wholesale is fine).
//! - Entries lapse after [`TTL`] regardless, so a write path that forgets
//!   to evict stretches revocation by at most TTL, never by the full
//!   access-token lifetime.
//! - Token expiry needs no handling here: JWT `exp` is verified before the
//!   cache is consulted, and only `typ == "access"` tokens reach it.

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

/// Upper bound on revocation staleness should an eviction path be missed.
const TTL: Duration = Duration::from_secs(60);

/// Insert-time prune threshold. Rotated-out jtis are usually removed by the
/// `clear()` on refresh, but login-minted ones linger; pruning on growth
/// keeps the map bounded without a sweeper task.
const PRUNE_LEN: usize = 128;

pub struct SessionCache {
    ttl: Duration,
    /// jti → time it was last confirmed against the DB. Guarded by a std
    /// (not tokio) lock: the critical sections are pure map operations and
    /// never held across an await.
    entries: RwLock<HashMap<String, Instant>>,
}

impl SessionCache {
    pub fn new() -> Self {
        Self::with_ttl(TTL)
    }

    fn with_ttl(ttl: Duration) -> Self {
        Self {
            ttl,
            entries: RwLock::new(HashMap::new()),
        }
    }

    /// True if `jti` was confirmed against the DB less than TTL ago.
    pub fn check(&self, jti: &str) -> bool {
        self.entries
            .read()
            .expect("session cache lock poisoned")
            .get(jti)
            .is_some_and(|confirmed| confirmed.elapsed() < self.ttl)
    }

    /// Record a DB-confirmed jti.
    pub fn insert(&self, jti: &str) {
        let mut entries = self.entries.write().expect("session cache lock poisoned");
        if entries.len() >= PRUNE_LEN {
            let ttl = self.ttl;
            entries.retain(|_, confirmed| confirmed.elapsed() < ttl);
        }
        entries.insert(jti.to_string(), Instant::now());
    }

    /// Drop a single jti (logout).
    pub fn evict(&self, jti: &str) {
        self.entries
            .write()
            .expect("session cache lock poisoned")
            .remove(jti);
    }

    /// Drop everything (refresh rotation, device revocation).
    pub fn clear(&self) {
        self.entries
            .write()
            .expect("session cache lock poisoned")
            .clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn miss_until_inserted_then_hit() {
        let cache = SessionCache::new();
        assert!(!cache.check("a"));
        cache.insert("a");
        assert!(cache.check("a"));
        assert!(!cache.check("b"));
    }

    #[test]
    fn entries_lapse_after_ttl() {
        let cache = SessionCache::with_ttl(Duration::ZERO);
        cache.insert("a");
        assert!(!cache.check("a"));
    }

    #[test]
    fn evict_drops_only_the_given_jti() {
        let cache = SessionCache::new();
        cache.insert("a");
        cache.insert("b");
        cache.evict("a");
        assert!(!cache.check("a"));
        assert!(cache.check("b"));
    }

    #[test]
    fn clear_drops_everything() {
        let cache = SessionCache::new();
        cache.insert("a");
        cache.insert("b");
        cache.clear();
        assert!(!cache.check("a"));
        assert!(!cache.check("b"));
    }

    #[test]
    fn insert_prunes_lapsed_entries_past_threshold() {
        let cache = SessionCache::with_ttl(Duration::ZERO);
        for i in 0..PRUNE_LEN {
            cache.insert(&format!("jti-{i}"));
        }
        cache.insert("fresh");
        let len = cache.entries.read().unwrap().len();
        assert_eq!(len, 1, "stale entries should have been pruned on insert");
    }
}
