/// Engine limits for the Community Edition.
///
/// These caps ensure the managed cloud offering remains meaningfully faster
/// and more capable. They apply automatically — no configuration can exceed them.
///
/// To lift all limits, use the Durable managed service or contact hello@durable.dev
/// for a commercial license.
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Maximum events persisted per second across the entire store.
pub const MAX_EVENTS_PER_SECOND: u32 = 10_000;

/// Maximum rows per TiDB batch INSERT.
/// The managed version uses 256; smaller batches mean more round-trips.
pub const MAX_BATCH_SIZE: usize = 64;

/// Minimum time between batch flushes.
/// The managed version can flush as fast as 10 ms.
pub const MIN_FLUSH_INTERVAL_MS: u64 = 100;

/// Approximate per-second rate limiter using two atomics.
///
/// The window boundary has a narrow race (two goroutines can reset the counter
/// simultaneously) which is intentional — the limit is a soft cap, not a hard
/// quota, and the approximation is negligible at the enforced throughput ceiling.
#[derive(Debug, Default)]
pub struct RateLimiter {
    window_start: AtomicU64,
    count: AtomicU32,
}

impl RateLimiter {
    /// Returns `true` if this call is within the per-second limit.
    pub fn allow(&self, limit: u32) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let prev = self.window_start.load(Ordering::Relaxed);
        if now != prev {
            self.window_start.store(now, Ordering::Relaxed);
            self.count.store(1, Ordering::Relaxed);
            return true;
        }
        self.count.fetch_add(1, Ordering::Relaxed) < limit
    }
}
