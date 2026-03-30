use std::sync::Arc;
use std::time::Duration;

use async_nats::{jetstream, Client as NatsClient};
use serde::Serialize;
use sqlx::{mysql::MySqlPoolOptions, MySql, MySqlPool, QueryBuilder};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio_retry::strategy::ExponentialBackoff;
use tracing::{debug, info, warn};

use crate::instance_lock::InstanceLock;
use crate::limits::RateLimiter;
use crate::{license, limits};

#[derive(Debug)]
pub struct SkialithStore {
    nats: NatsClient,
    js: jetstream::Context,
    tidb: MySqlPool,
    enqueue_tx: mpsc::Sender<PendingEvent>,
    dead_letter_tx: Option<mpsc::Sender<Vec<PendingEvent>>>,
    rate_limiter: Arc<RateLimiter>,
    /// Effective events/sec ceiling, set from the license at construction time.
    rate_limit: u32,
    /// Keeps the NATS KV instance lock alive for the process lifetime.
    _instance_lock: Option<InstanceLock>,
}

#[derive(Debug, Clone)]
pub struct SkialithConfig {
    pub max_batch_size: usize,
    pub max_batch_linger: Duration,
    pub channel_capacity: usize,
}

impl Default for SkialithConfig {
    fn default() -> Self {
        Self {
            max_batch_size: 256,
            max_batch_linger: Duration::from_millis(250),
            channel_capacity: 10_000,
        }
    }
}

#[derive(Debug, Error)]
pub enum SkialithError {
    #[error("failed to serialize event payload")]
    Serialize(#[from] serde_json::Error),

    #[error("nats connect failed")]
    NatsConnect(#[from] async_nats::ConnectError),

    #[error("nats publish failed")]
    NatsPublish(#[from] async_nats::PublishError),

    #[error("jetstream publish failed")]
    JetStreamPublish(#[from] async_nats::jetstream::context::PublishError),

    #[error("jetstream stream error")]
    JetStream(async_nats::jetstream::context::CreateStreamError),

    #[error("tidb write failed")]
    Sqlx(#[from] sqlx::Error),

    #[error("event queue is closed")]
    QueueClosed,

    #[error("instance lock: {0}")]
    InstanceLock(String),
}

#[derive(Debug, Clone)]
pub struct PendingEvent {
    agent_id: String,
    event_id: String,
    payload_json: Vec<u8>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentTraceEvent<'a, T: Serialize> {
    pub agent_id: &'a str,
    pub event_id: &'a str,
    pub payload: &'a T,
}

impl SkialithStore {
    pub fn new(
        nats: NatsClient,
        tidb: MySqlPool,
        cfg: SkialithConfig,
        rate_limit: u32,
        instance_lock: Option<InstanceLock>,
    ) -> Self {
        let cfg = SkialithConfig {
            max_batch_size: cfg.max_batch_size.min(limits::MAX_BATCH_SIZE),
            max_batch_linger: cfg
                .max_batch_linger
                .max(Duration::from_millis(limits::MIN_FLUSH_INTERVAL_MS)),
            ..cfg
        };
        if !limits::is_managed() {
            warn!(
                rate_limit,
                max_batch_size = cfg.max_batch_size,
                flush_interval_ms = cfg.max_batch_linger.as_millis(),
                "Community Edition limits active"
            );
        }

        let (enqueue_tx, enqueue_rx) = mpsc::channel(cfg.channel_capacity);
        tokio::spawn(run_tidb_batch_writer(tidb.clone(), cfg, enqueue_rx, None));

        let js = jetstream::new(nats.clone());
        Self {
            nats,
            js,
            tidb,
            enqueue_tx,
            dead_letter_tx: None,
            rate_limiter: Arc::new(RateLimiter::default()),
            rate_limit,
            _instance_lock: instance_lock,
        }
    }

    pub fn with_dead_letter(mut self, tx: mpsc::Sender<Vec<PendingEvent>>) -> Self {
        self.dead_letter_tx = Some(tx);
        let (enqueue_tx, enqueue_rx) = mpsc::channel(10_000);
        tokio::spawn(run_tidb_batch_writer(
            self.tidb.clone(),
            SkialithConfig::default(),
            enqueue_rx,
            self.dead_letter_tx.clone(),
        ));
        self.enqueue_tx = enqueue_tx;
        self
    }

    /// Construct directly without connecting to NATS/TiDB. Useful for tests.
    /// Applies the community rate limit; no instance lock is acquired.
    pub fn new_unchecked(nats: NatsClient, tidb: MySqlPool, cfg: SkialithConfig) -> Self {
        Self::new(nats, tidb, cfg, limits::COMMUNITY_RATE_LIMIT, None)
    }

    pub async fn connect(
        nats_url: &str,
        tidb_url: &str,
        cfg: SkialithConfig,
    ) -> Result<Self, SkialithError> {
        let enterprise_license = license::load();
        let rate_limit = license::effective_rate_limit(&enterprise_license);
        let ha_allowed = license::allows_ha(&enterprise_license);

        match &enterprise_license {
            Some(lic) => eprintln!(
                "Skialith {} — licensed to {} (rate limit: {}/sec, HA: {})",
                env!("CARGO_PKG_VERSION"),
                lic.customer_id,
                if rate_limit == u32::MAX { "unlimited".to_string() } else { rate_limit.to_string() },
                if ha_allowed { "enabled" } else { "disabled" },
            ),
            None if limits::is_managed() => eprintln!(
                "Skialith {} (Managed Edition — no license key, community limits apply)\n\
                 Set SKIALITH_LICENSE_KEY to unlock full throughput.",
                env!("CARGO_PKG_VERSION"),
            ),
            None => eprintln!(
                "Skialith {} (Community Edition)\n\
                 License : BUSL 1.1 — self-hosting in your own VPC is free.\n\
                 Limits  : {}/sec · {} row batches · {}ms min flush\n\
                 Upgrade : hello@durable.dev",
                env!("CARGO_PKG_VERSION"),
                limits::COMMUNITY_RATE_LIMIT,
                limits::MAX_BATCH_SIZE,
                limits::MIN_FLUSH_INTERVAL_MS,
            ),
        }

        let nats = async_nats::connect(nats_url).await?;
        let js = jetstream::new(nats.clone());

        let instance_lock = InstanceLock::acquire(&js, ha_allowed)
            .await
            .map_err(|e| SkialithError::InstanceLock(e.to_string()))?;

        let tidb = MySqlPoolOptions::new().connect(tidb_url).await?;
        Ok(Self::new(nats, tidb, cfg, rate_limit, Some(instance_lock)))
    }

    pub async fn save_event<T: Serialize + Send + Sync>(
        &self,
        agent_id: &str,
        event_id: &str,
        payload: &T,
    ) -> Result<(), SkialithError> {
        // Block until a slot is available. Limit is set from the enterprise
        // license at construction time; defaults to COMMUNITY_RATE_LIMIT (1k/sec).
        self.rate_limiter.wait(self.rate_limit).await;

        let subject = format!("agent.trace.{agent_id}");
        let envelope = AgentTraceEvent {
            agent_id,
            event_id,
            payload,
        };
        let payload_json = serde_json::to_vec(&envelope)?;

        self.js
            .publish(subject, payload_json.clone().into())
            .await?
            .await?;
        debug!(agent_id, event_id, "event published to NATS");

        let pending = PendingEvent {
            agent_id: agent_id.to_string(),
            event_id: event_id.to_string(),
            payload_json,
        };

        self.enqueue_tx
            .send(pending)
            .await
            .map_err(|_| SkialithError::QueueClosed)?;

        Ok(())
    }

    pub fn tidb_pool(&self) -> &MySqlPool {
        &self.tidb
    }
}

pub async fn ensure_stream(
    js: &jetstream::Context,
    stream_name: &str,
    subjects: Vec<String>,
) -> Result<(), SkialithError> {
    js.get_or_create_stream(jetstream::stream::Config {
        name: stream_name.to_string(),
        subjects,
        ..Default::default()
    })
    .await
    .map_err(SkialithError::JetStream)?;
    Ok(())
}

fn retry_strategy() -> impl Iterator<Item = Duration> {
    ExponentialBackoff::from_millis(100).factor(2).take(3)
}

async fn flush_batch_with_retry(
    tidb: &MySqlPool,
    batch: &mut Vec<PendingEvent>,
    dead_letter_tx: &Option<mpsc::Sender<Vec<PendingEvent>>>,
) {
    let delays: Vec<Duration> = retry_strategy().collect();
    let total_attempts = delays.len() + 1;

    for (attempt, delay) in delays.iter().enumerate() {
        match flush_batch(tidb, batch).await {
            Ok(()) => return,
            Err(e) => {
                eprintln!(
                    "[skialith_store] TiDB batch write attempt {}/{total_attempts} failed: {e}",
                    attempt + 1
                );
                tokio::time::sleep(*delay).await;
            }
        }
    }

    // Final attempt after all delays exhausted.
    if flush_batch(tidb, batch).await.is_ok() {
        return;
    }

    // All attempts failed: route to dead-letter or log the loss.
    // `flush_batch` only clears the batch on success, so it still contains the events.
    eprintln!("[skialith_store] TiDB batch write failed after {total_attempts} attempts; routing to dead-letter");
    let failed = std::mem::take(batch);
    match dead_letter_tx {
        Some(tx) => {
            if tx.send(failed).await.is_err() {
                eprintln!("[skialith_store] dead-letter channel is closed; events lost");
            }
        }
        None => {
            eprintln!(
                "[skialith_store] no dead-letter channel configured; {} events lost",
                failed.len()
            );
        }
    }
}

async fn run_tidb_batch_writer(
    tidb: MySqlPool,
    cfg: SkialithConfig,
    mut rx: mpsc::Receiver<PendingEvent>,
    dead_letter_tx: Option<mpsc::Sender<Vec<PendingEvent>>>,
) {
    let mut batch: Vec<PendingEvent> = Vec::with_capacity(cfg.max_batch_size);
    let mut ticker = tokio::time::interval(cfg.max_batch_linger);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            maybe_ev = rx.recv() => {
                match maybe_ev {
                    Some(ev) => {
                        batch.push(ev);
                        debug!(queue_depth = batch.len(), "event enqueued");
                        if batch.len() >= cfg.max_batch_size {
                            flush_batch_with_retry(&tidb, &mut batch, &dead_letter_tx).await;
                        }
                    }
                    None => {
                        if !batch.is_empty() {
                            flush_batch_with_retry(&tidb, &mut batch, &dead_letter_tx).await;
                        }
                        break;
                    }
                }
            }
            _ = ticker.tick() => {
                if !batch.is_empty() {
                    flush_batch_with_retry(&tidb, &mut batch, &dead_letter_tx).await;
                }
            }
        }
    }
}

async fn flush_batch(tidb: &MySqlPool, batch: &mut Vec<PendingEvent>) -> Result<(), sqlx::Error> {
    debug!(batch_size = batch.len(), "flushing batch to TiDB");
    let mut tx = tidb.begin().await?;

    let mut qb: QueryBuilder<MySql> =
        QueryBuilder::new("INSERT INTO agent_events (agent_id, event_id, payload_json) ");

    qb.push_values(batch.iter(), |mut b, ev| {
        let payload_str = String::from_utf8_lossy(&ev.payload_json).into_owned();
        b.push_bind(&ev.agent_id)
            .push_bind(&ev.event_id)
            .push_bind(payload_str);
    });

    qb.build().execute(&mut *tx).await?;
    tx.commit().await?;
    info!(batch_size = batch.len(), "batch committed to TiDB");

    batch.clear();
    Ok(())
}
