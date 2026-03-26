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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Community Edition throughput ceiling — always 1,000, regardless of build flags.
///
/// Raising this limit requires a valid enterprise license key (`DURABLE_LICENSE_KEY`).
/// The `--features managed` flag alone is not sufficient; see `crate::license`.
///
/// Conversion trigger: 50 concurrent agents × ~3 events/step each = ~1k/sec.
/// At that scale the team has real revenue and the managed tier makes sense.
pub const COMMUNITY_RATE_LIMIT: u32 = 1_000;

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

/// Per-second rate limiter with backpressure.
///
/// Unlike a hard-reject limiter, `wait()` blocks the caller until a slot is
/// available in the current one-second window. Events are never dropped —
/// callers that exceed the cap are slowed down until the next window opens.
///
/// This makes Community Edition lossless but bounded in throughput. The
/// managed build sets `MAX_EVENTS_PER_SECOND = u32::MAX`, so `wait()` is a
/// no-op and callers return immediately at full NATS speed.
///
/// Implementation: two atomics — the current window epoch (seconds) and the
/// slot counter. Slot claims use compare-and-exchange to avoid double-counting.
/// The window boundary has a narrow reset race that is intentional: the limit
/// is a soft throughput cap, not a cryptographic quota.
#[derive(Debug, Default)]
pub struct RateLimiter {
    window_start: AtomicU64,
    count: AtomicU32,
}

impl RateLimiter {
    /// Block until a slot is available in the current one-second window.
    /// Returns immediately if the managed feature is active.
    pub async fn wait(&self, limit: u32) {
        if limit == u32::MAX {
            return;
        }
        loop {
            let now_secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let prev = self.window_start.load(Ordering::Relaxed);

            if now_secs != prev {
                // New window: reset counter and claim this slot.
                self.window_start.store(now_secs, Ordering::Relaxed);
                self.count.store(1, Ordering::Relaxed);
                return;
            }

            let c = self.count.load(Ordering::Relaxed);
            if c < limit {
                // Atomically claim a slot. If another task beat us to it, retry.
                if self
                    .count
                    .compare_exchange(c, c + 1, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok()
                {
                    return;
                }
                continue;
            }

            // Window is full. Sleep until the start of the next second.
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            let sleep_ms = 1001 - (now_ms % 1000);
            tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
        }
    }
}
