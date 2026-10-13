//! The account quota behind `statfs`.
//!
//! The engine's `daemon.status` reply carries an optional `quota` object
//! (`used_bytes`, `total_bytes`, `fetched_at_ms`, `stale`, `error`; any value
//! may be null, the whole object is absent for providers without an account
//! quota). `statfs` must answer at once, so it reads a cache here and a
//! background task refreshes it; with no usable value it reports the fixed
//! fallback volume.

use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Block size reported by `statfs` (`bsize` and `frsize`).
pub const BLOCK_SIZE: u64 = 4096;
/// Blocks of the fallback volume: 1<<32 blocks of 4 KiB, about 16 TiB, all free.
pub const FALLBACK_BLOCKS: u64 = 1 << 32;
/// Default time between quota refreshes.
pub const DEFAULT_REFRESH_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quota {
    pub used_bytes: u64,
    pub total_bytes: u64,
}

impl Quota {
    /// The quota of a `daemon.status` reply. `None` when the reply has no
    /// `quota` object or its used/total are unknown (null, negative, or a
    /// total of zero): an unknown is never read as zero.
    pub fn from_status(reply: &Value) -> Option<Quota> {
        let q = reply.get("quota")?;
        let used = q.get("used_bytes")?.as_u64()?;
        let total = q.get("total_bytes")?.as_u64()?;
        if total == 0 {
            return None;
        }
        Some(Quota { used_bytes: used, total_bytes: total })
    }

    /// `(blocks, bfree)` for `statfs`; free is clamped at zero when the
    /// account is over quota.
    pub fn blocks(&self) -> (u64, u64) {
        let total = self.total_bytes / BLOCK_SIZE;
        let free = self.total_bytes.saturating_sub(self.used_bytes) / BLOCK_SIZE;
        (total, free)
    }
}

/// `(blocks, bfree)` for a possibly unknown quota.
pub fn statfs_blocks(quota: Option<Quota>) -> (u64, u64) {
    match quota {
        Some(q) => q.blocks(),
        None => (FALLBACK_BLOCKS, FALLBACK_BLOCKS),
    }
}

#[derive(Default)]
struct State {
    quota: Option<Quota>,
    last_attempt: Option<Instant>,
    in_flight: bool,
}

/// Shared quota cache. Cloning shares the state.
#[derive(Clone)]
pub struct QuotaCache {
    state: Arc<Mutex<State>>,
    interval: Duration,
}

impl QuotaCache {
    pub fn new(interval: Duration) -> Self {
        Self { state: Arc::new(Mutex::new(State::default())), interval }
    }

    /// The last known quota, if any.
    pub fn current(&self) -> Option<Quota> {
        self.state.lock().expect("quota state").quota
    }

    /// Claim a refresh when none is running and the last attempt is older
    /// than the interval (or there was none). The caller that gets `true`
    /// must call [`finish_refresh`](Self::finish_refresh).
    pub fn begin_refresh(&self) -> bool {
        let mut s = self.state.lock().expect("quota state");
        if s.in_flight {
            return false;
        }
        if s.last_attempt.is_some_and(|t| t.elapsed() < self.interval) {
            return false;
        }
        s.in_flight = true;
        s.last_attempt = Some(Instant::now());
        true
    }

    /// Record the outcome of a refresh. `None` (fetch failed or the quota is
    /// unknown) keeps the last known value.
    pub fn finish_refresh(&self, fetched: Option<Quota>) {
        let mut s = self.state.lock().expect("quota state");
        s.in_flight = false;
        if fetched.is_some() {
            s.quota = fetched;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn free_blocks_clamp_to_zero_when_over_quota() {
        let q = Quota { used_bytes: 10 * BLOCK_SIZE, total_bytes: 4 * BLOCK_SIZE };
        assert_eq!(q.blocks(), (4, 0));
    }

    #[test]
    fn unknown_quota_is_never_read_as_zero() {
        for q in [
            json!({}),
            json!({"quota": null}),
            json!({"quota": {"used_bytes": null, "total_bytes": 5}}),
            json!({"quota": {"used_bytes": 5, "total_bytes": null}}),
            json!({"quota": {"used_bytes": 5, "total_bytes": 0}}),
            json!({"quota": {"used_bytes": -1, "total_bytes": 5}}),
        ] {
            assert_eq!(Quota::from_status(&q), None, "{q}");
        }
    }

    #[test]
    fn a_stale_quota_is_still_used() {
        let q = json!({"quota": {"used_bytes": 1, "total_bytes": 9, "stale": true, "error": "x"}});
        assert_eq!(Quota::from_status(&q), Some(Quota { used_bytes: 1, total_bytes: 9 }));
    }

    #[test]
    fn a_failed_refresh_keeps_the_last_known_quota() {
        let c = QuotaCache::new(Duration::ZERO);
        let q = Quota { used_bytes: 1, total_bytes: 2 };
        assert!(c.begin_refresh());
        c.finish_refresh(Some(q));
        assert!(c.begin_refresh());
        c.finish_refresh(None);
        assert_eq!(c.current(), Some(q));
    }

    #[test]
    fn only_one_refresh_runs_at_a_time() {
        let c = QuotaCache::new(Duration::ZERO);
        assert!(c.begin_refresh());
        assert!(!c.begin_refresh());
        c.finish_refresh(None);
        assert!(c.begin_refresh());
    }
}
