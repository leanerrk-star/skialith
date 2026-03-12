use std::time::Duration;

use async_nats::Client as NatsClient;
use serde::Serialize;
use sqlx::{mysql::MySqlPoolOptions, MySql, MySqlPool, QueryBuilder};
use thiserror::Error;
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub struct DurableEventStore {
    nats: NatsClient,
    tidb: MySqlPool,
    enqueue_tx: mpsc::Sender<PendingEvent>,
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
struct PendingEvent {
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
        tokio::spawn(run_tidb_batch_writer(tidb.clone(), cfg, enqueue_rx));

        Self {
            nats,
            tidb,
            enqueue_tx,
        }
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
        let subject = format!("agent.{agent_id}.trace");
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

async fn run_tidb_batch_writer(
    tidb: MySqlPool,
    cfg: DurableEventStoreConfig,
    mut rx: mpsc::Receiver<PendingEvent>,
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
                            if let Err(e) = flush_batch(&tidb, &mut batch).await {
                                // In a production system you would route this to your logging/metrics.
                                // We intentionally avoid panicking to keep the worker alive.
                                let _ = e;
                            }
                        }
                    }
                    None => {
                        // Channel closed: flush whatever we have and exit.
                        if !batch.is_empty() {
                            let _ = flush_batch(&tidb, &mut batch).await;
                        }
                        break;
                    }
                }
            }
            _ = ticker.tick() => {
                if !batch.is_empty() {
                    if let Err(e) = flush_batch(&tidb, &mut batch).await {
                        let _ = e;
                    }
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

