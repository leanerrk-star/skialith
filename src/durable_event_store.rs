use std::time::Duration;

use async_nats::Client as NatsClient;
use serde::Serialize;
use sqlx::{mysql::MySqlPoolOptions, MySql, MySqlPool, QueryBuilder};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio_retry::strategy::ExponentialBackoff;

#[derive(Debug, Clone)]
pub struct DurableEventStore {
    nats: NatsClient,
    tidb: MySqlPool,
    enqueue_tx: mpsc::Sender<PendingEvent>,
    dead_letter_tx: Option<mpsc::Sender<Vec<PendingEvent>>>,
}

#[derive(Debug, Clone)]
pub struct DurableEventStoreConfig {
    /// Maximum number of events to batch into a single TiDB INSERT.
    pub max_batch_size: usize,
    /// Maximum time to wait before flushing a non-empty batch.
    pub max_batch_linger: Duration,
    /// Channel capacity between `save_event` and the TiDB batch writer.
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

    #[error("tidb write failed")]
    Sqlx(#[from] sqlx::Error),

    #[error("event queue is closed")]
    QueueClosed,
}

#[derive(Debug, Clone)]
pub struct PendingEvent {
    agent_id: String,
    event_id: String,
    payload_json: Vec<u8>,
}

/// A serializable event wrapper. Your domain event type can be embedded in `payload`.
#[derive(Debug, Clone, Serialize)]
pub struct AgentTraceEvent<'a, T: Serialize> {
    pub agent_id: &'a str,
    pub event_id: &'a str,
    pub payload: &'a T,
}

impl DurableEventStore {
    /// Create a new event store from an existing NATS connection and TiDB pool.
    ///
    /// TiDB table expectation (one possible schema):
    /// - `agent_events(agent_id VARCHAR(..), event_id VARCHAR(..), payload_json JSON, created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP)`
    pub fn new(nats: NatsClient, tidb: MySqlPool, cfg: DurableEventStoreConfig) -> Self {
        let (enqueue_tx, enqueue_rx) = mpsc::channel(cfg.channel_capacity);
        tokio::spawn(run_tidb_batch_writer(tidb.clone(), cfg, enqueue_rx, None));

        Self {
            nats,
            tidb,
            enqueue_tx,
            dead_letter_tx: None,
        }
    }

    /// Attach a dead-letter channel that receives batches of events that could not be
    /// written to TiDB after all retries are exhausted.
    pub fn with_dead_letter(mut self, tx: mpsc::Sender<Vec<PendingEvent>>) -> Self {
        self.dead_letter_tx = Some(tx);
        // Re-spawn the batch writer with the dead-letter sender.
        // Note: the previously spawned writer (from `new`) will exit once the old
        // enqueue channel's receiver half is dropped here. We create a fresh channel.
        let (enqueue_tx, enqueue_rx) = mpsc::channel(
            // We cannot retrieve the original capacity after the channel is created,
            // so we use the default capacity for the replacement channel.
            10_000,
        );
        tokio::spawn(run_tidb_batch_writer(
            self.tidb.clone(),
            DurableEventStoreConfig::default(),
            enqueue_rx,
            self.dead_letter_tx.clone(),
        ));
        self.enqueue_tx = enqueue_tx;
        self
    }

    /// Convenience constructor if you want this struct to own connection setup.
    pub async fn connect(
        nats_url: &str,
        tidb_url: &str,
        cfg: DurableEventStoreConfig,
    ) -> Result<Self, DurableEventStoreError> {
        let nats = async_nats::connect(nats_url).await?;
        let tidb = MySqlPoolOptions::new().connect(tidb_url).await?;
        Ok(Self::new(nats, tidb, cfg))
    }

    /// Publish the event to NATS (write-ahead log) and enqueue for batched TiDB persistence.
    ///
    /// NATS subject format: `agent.{agent_id}.trace`
    pub async fn save_event<T: Serialize + Send + Sync>(
        &self,
        agent_id: &str,
        event_id: &str,
        payload: &T,
    ) -> Result<(), DurableEventStoreError> {
        let subject = format!("agent.trace.{agent_id}");
        let envelope = AgentTraceEvent {
            agent_id,
            event_id,
            payload,
        };
        let payload_json = serde_json::to_vec(&envelope)?;

        self.nats
            .publish(subject, payload_json.clone().into())
            .await?;

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

    /// Exposes the underlying pool for migrations / health checks if needed.
    pub fn tidb_pool(&self) -> &MySqlPool {
        &self.tidb
    }
}

/// Retry strategy: exponential backoff starting at 100 ms, factor 2, max 3 retries.
fn retry_strategy() -> impl Iterator<Item = Duration> {
    ExponentialBackoff::from_millis(100).factor(2).take(3)
}

async fn flush_batch_with_retry(
    tidb: &MySqlPool,
    batch: &mut Vec<PendingEvent>,
    dead_letter_tx: &Option<mpsc::Sender<Vec<PendingEvent>>>,
) {
    // Manual retry loop: ExponentialBackoff gives us the sleep durations between
    // attempts. We can't use Retry::spawn because it requires a closure returning
    // a Future, and futures can't hold &mut references across closure boundaries.
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
                        if batch.len() >= cfg.max_batch_size {
                            flush_batch_with_retry(&tidb, &mut batch, &dead_letter_tx).await;
                        }
                    }
                    None => {
                        // Channel closed: flush whatever we have and exit.
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
    let mut tx = tidb.begin().await?;

    // Multi-row insert; TiDB supports this efficiently.
    // If your `payload_json` column is JSON, TiDB will accept a JSON string.
    let mut qb: QueryBuilder<MySql> =
        QueryBuilder::new("INSERT INTO agent_events (agent_id, event_id, payload_json) ");

    qb.push_values(batch.iter(), |mut b, ev| {
        // `serde_json::to_vec` always produces UTF-8 JSON.
        // Still, we convert defensively without panicking.
        let payload_str = String::from_utf8_lossy(&ev.payload_json).into_owned();
        b.push_bind(&ev.agent_id)
            .push_bind(&ev.event_id)
            .push_bind(payload_str);
    });

    qb.build().execute(&mut *tx).await?;
    tx.commit().await?;

    batch.clear();
    Ok(())
}
