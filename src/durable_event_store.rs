use std::sync::Arc;
use std::time::Duration;

use async_nats::{jetstream, Client as NatsClient};
use serde::Serialize;
use sqlx::{mysql::MySqlPoolOptions, MySql, MySqlPool, QueryBuilder};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio_retry::strategy::ExponentialBackoff;
use tracing::{debug, info, warn};

use crate::limits::{self, RateLimiter};

#[derive(Debug, Clone)]
pub struct DurableEventStore {
    nats: NatsClient,
    js: jetstream::Context,
    tidb: MySqlPool,
    enqueue_tx: mpsc::Sender<PendingEvent>,
    dead_letter_tx: Option<mpsc::Sender<Vec<PendingEvent>>>,
    rate_limiter: Arc<RateLimiter>,
}

#[derive(Debug, Clone)]
pub struct DurableEventStoreConfig {
    pub max_batch_size: usize,
    pub max_batch_linger: Duration,
    pub channel_capacity: usize,
}

impl Default for DurableEventStoreConfig {
    fn default() -> Self {
        Self {
            max_batch_size: 256,
            max_batch_linger: Duration::from_millis(250),
            channel_capacity: 10_000,
        }
    }
}

#[derive(Debug, Error)]
pub enum DurableEventStoreError {
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

    #[error("rate limit exceeded ({0} events/sec); upgrade to the managed service for higher throughput")]
    RateLimited(u32),
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

impl DurableEventStore {
    pub fn new(nats: NatsClient, tidb: MySqlPool, cfg: DurableEventStoreConfig) -> Self {
        // Apply Community Edition caps. The managed service removes these.
        let cfg = DurableEventStoreConfig {
            max_batch_size: cfg.max_batch_size.min(limits::MAX_BATCH_SIZE),
            max_batch_linger: cfg
                .max_batch_linger
                .max(Duration::from_millis(limits::MIN_FLUSH_INTERVAL_MS)),
            ..cfg
        };
        if !limits::is_managed() {
            warn!(
                max_batch_size = cfg.max_batch_size,
                flush_interval_ms = cfg.max_batch_linger.as_millis(),
                "Community Edition limits active — upgrade to managed for full throughput"
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
        }
    }

    pub fn with_dead_letter(mut self, tx: mpsc::Sender<Vec<PendingEvent>>) -> Self {
        self.dead_letter_tx = Some(tx);
        let (enqueue_tx, enqueue_rx) = mpsc::channel(10_000);
        tokio::spawn(run_tidb_batch_writer(
            self.tidb.clone(),
            DurableEventStoreConfig::default(),
            enqueue_rx,
            self.dead_letter_tx.clone(),
        ));
        self.enqueue_tx = enqueue_tx;
        self
    }

    pub async fn connect(
        nats_url: &str,
        tidb_url: &str,
        cfg: DurableEventStoreConfig,
    ) -> Result<Self, DurableEventStoreError> {
        if limits::is_managed() {
            eprintln!(
                "Durable Agent Core {} (Managed Edition)",
                env!("CARGO_PKG_VERSION"),
            );
        } else {
            eprintln!(
                "Durable Agent Core {} (Community Edition)\n\
                 License : BUSL 1.1 — self-hosting in your own VPC is free.\n\
                 Limits  : {} events/sec · {} row batches · {}ms min flush interval\n\
                 Upgrade : hello@durable.dev",
                env!("CARGO_PKG_VERSION"),
                limits::MAX_EVENTS_PER_SECOND,
                limits::MAX_BATCH_SIZE,
                limits::MIN_FLUSH_INTERVAL_MS,
            );
        }
        let nats = async_nats::connect(nats_url).await?;
        let tidb = MySqlPoolOptions::new().connect(tidb_url).await?;
        Ok(Self::new(nats, tidb, cfg))
    }

    pub async fn save_event<T: Serialize + Send + Sync>(
        &self,
        agent_id: &str,
        event_id: &str,
        payload: &T,
    ) -> Result<(), DurableEventStoreError> {
        if !self.rate_limiter.allow(limits::MAX_EVENTS_PER_SECOND) {
            return Err(DurableEventStoreError::RateLimited(limits::MAX_EVENTS_PER_SECOND));
        }

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
            .map_err(|_| DurableEventStoreError::QueueClosed)?;

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
) -> Result<(), DurableEventStoreError> {
    js.get_or_create_stream(jetstream::stream::Config {
        name: stream_name.to_string(),
        subjects,
        ..Default::default()
    })
    .await
    .map_err(DurableEventStoreError::JetStream)?;
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
                    "[durable_event_store] TiDB batch write attempt {}/{total_attempts} failed: {e}",
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
    eprintln!("[durable_event_store] TiDB batch write failed after {total_attempts} attempts; routing to dead-letter");
    let failed = std::mem::take(batch);
    match dead_letter_tx {
        Some(tx) => {
            if tx.send(failed).await.is_err() {
                eprintln!("[durable_event_store] dead-letter channel is closed; events lost");
            }
        }
        None => {
            eprintln!(
                "[durable_event_store] no dead-letter channel configured; {} events lost",
                failed.len()
            );
        }
    }
}

async fn run_tidb_batch_writer(
    tidb: MySqlPool,
    cfg: DurableEventStoreConfig,
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
