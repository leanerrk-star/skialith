/// Engine limits.
///
/// Community Edition (default): hard caps that keep self-hosted deployments
/// within a reasonable envelope and ensure the managed service remains
/// meaningfully faster.
///
/// Managed Edition (`--features managed`): all caps removed. This feature is
/// never enabled in OSS releases; it is activated only in the private managed
/// service build pipeline.
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(not(feature = "managed"))]
pub const MAX_EVENTS_PER_SECOND: u32 = 10_000;
#[cfg(feature = "managed")]
pub const MAX_EVENTS_PER_SECOND: u32 = u32::MAX;

/// Max rows per TiDB batch INSERT. Managed uses 256 — 4× larger batches,
/// proportionally fewer round-trips.
#[cfg(not(feature = "managed"))]
pub const MAX_BATCH_SIZE: usize = 64;
#[cfg(feature = "managed")]
pub const MAX_BATCH_SIZE: usize = usize::MAX;

/// Minimum flush interval. Managed can flush as fast as 10 ms.
#[cfg(not(feature = "managed"))]
pub const MIN_FLUSH_INTERVAL_MS: u64 = 100;
#[cfg(feature = "managed")]
pub const MIN_FLUSH_INTERVAL_MS: u64 = 0;

/// Returns true if this binary was compiled with the `managed` feature.
pub const fn is_managed() -> bool {
    cfg!(feature = "managed")
}

/// Approximate per-second rate limiter using two atomics.
///
/// The window boundary has a narrow race where two threads can reset the
/// counter simultaneously; this is intentional — the limit is a soft cap,
/// not a hard quota, and the approximation is negligible at these rates.
#[derive(Debug, Default)]
pub struct RateLimiter {
    window_start: AtomicU64,
    count: AtomicU32,
}

impl RateLimiter {
    pub fn allow(&self, limit: u32) -> bool {
        if limit == u32::MAX {
            return true;
        }
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
