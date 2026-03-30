/// Single-instance enforcement for Community Edition.
///
/// Community Edition is not designed for HA. Running multiple instances would:
///   - Multiply the effective rate limit (each instance has its own in-process
///     limiter, so N instances = N × 1,000 events/sec).
///   - Create write races in the TiDB batch writer producing duplicate rows.
///   - Split the dead-letter channel across processes with no coordination.
///
/// On startup, Community Edition claims an atomic lock in NATS KV using a
/// compare-and-swap on revision 0 (key must not exist). A second instance that
/// finds the lock held refuses to start. The lock has a 30-second TTL; the
/// holder refreshes it every 10 seconds. If the holder dies the lock expires
/// naturally — recovery takes at most 30 seconds with no automated failover.
///
/// Enterprise licenses with `allow_ha: true` bypass the lock entirely,
/// enabling coordinated multi-instance deployment backed by a distributed
/// rate limiter in the managed control plane.
use std::time::Duration;

use async_nats::jetstream;
use thiserror::Error;

const LOCK_BUCKET: &str = "skialith-community-instance";
const LOCK_KEY: &str = "lock";
const LOCK_TTL: Duration = Duration::from_secs(30);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Debug, Error)]
pub enum LockError {
    #[error(
        "Community Edition allows only one instance.\n\
         Another instance is already running (NATS KV lock held).\n\
         Running multiple instances bypasses the Community rate limit and risks duplicate writes.\n\
         Upgrade to an enterprise license for coordinated HA: hello@durable.dev"
    )]
    AlreadyRunning,

    #[error("NATS KV error acquiring instance lock: {0}")]
    Nats(String),
}

/// RAII guard that holds the single-instance NATS KV lock.
/// Dropping this aborts the heartbeat; the lock TTL then expires naturally.
#[derive(Debug)]
pub struct InstanceLock {
    _heartbeat: tokio::task::JoinHandle<()>,
}

impl InstanceLock {
    /// Claim the lock. Pass `skip = true` (enterprise HA license) to bypass.
    ///
    /// Uses `update(key, value, revision=0)` — NATS KV's atomic
    /// "create if not exists" primitive. Fails if any other instance
    /// currently holds the key.
    pub async fn acquire(js: &jetstream::Context, skip: bool) -> Result<Self, LockError> {
        if skip {
            return Ok(Self {
                _heartbeat: tokio::spawn(std::future::pending()),
            });
        }

        let kv = js
            .create_key_value(async_nats::jetstream::kv::Config {
                bucket: LOCK_BUCKET.to_string(),
                max_age: LOCK_TTL,
                history: 1,
                ..Default::default()
            })
            .await
            .map_err(|e| LockError::Nats(e.to_string()))?;

        // revision=0 means "succeed only if the key does not exist".
        kv.update(LOCK_KEY, "1".into(), 0)
            .await
            .map_err(|_| LockError::AlreadyRunning)?;

        let kv_hb = kv.clone();
        let heartbeat = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(HEARTBEAT_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut revision = 1u64;
            loop {
                ticker.tick().await;
                match kv_hb.update(LOCK_KEY, "1".into(), revision).await {
                    Ok(new_rev) => revision = new_rev,
                    Err(_) => break,
                }
            }
        });

        Ok(Self {
            _heartbeat: heartbeat,
        })
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        // Stop heartbeat. KV entry expires after LOCK_TTL without refresh.
        self._heartbeat.abort();
    }
}
