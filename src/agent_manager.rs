use async_nats::jetstream;
use bytes::Bytes;
use serde::Serialize;
use thiserror::Error;
use tracing::info;

#[derive(Debug, Clone)]
pub struct AgentManager {
    js: jetstream::Context,
}

#[derive(Debug, Error)]
pub enum AgentManagerError {
    #[error("failed to serialize checkpoint state")]
    Serialize(#[from] serde_json::Error),

    #[error("jetstream publish failed")]
    Publish(#[from] async_nats::jetstream::context::PublishError),

    #[error("jetstream ack failed")]
    Ack(#[from] async_nats::Error),
}

impl AgentManager {
    pub fn new(nats: async_nats::Client) -> Self {
        Self {
            js: jetstream::new(nats),
        }
    }

    /// Checkpoint an agent's state before any tool execution.
    ///
    /// This publishes to JetStream (WAL) and awaits the PubAck to ensure the write is committed
    /// server-side before returning.
    ///
    /// "High Priority" is encoded as headers so consumers/executors can enforce ordering:
    /// - `X-Priority: high`
    /// - `X-WAL-Phase: checkpoint`
    pub async fn checkpoint_state<T: Serialize + Send + Sync>(
        &self,
        agent_id: &str,
        state: &T,
    ) -> Result<(), AgentManagerError> {
        // Subject for checkpoints. This is distinct from trace events; adjust to your stream config.
        let subject = format!("agent.checkpoint.{agent_id}");

        let payload = serde_json::to_vec(state)?;
        let mut headers = async_nats::HeaderMap::new();
        headers.append("X-Priority", "high");
        headers.append("X-WAL-Phase", "checkpoint");
        headers.append("X-Agent-Id", agent_id);

        // Publish to JetStream and await PubAck to ensure WAL is updated.
        let ack_fut = self
            .js
            .publish_with_headers(subject, headers, Bytes::from(payload))
            .await?;
        ack_fut.await?;
        info!(agent_id, "checkpoint published to JetStream");

        Ok(())
    }
}

