use async_nats::jetstream;
use futures::StreamExt;
use serde_json::Value;
use sqlx::MySqlPool;
use thiserror::Error;

use crate::agent_trace::{AgentEventType, AgentTrace};

#[derive(Debug, Error)]
pub enum TraceIngestError {
    #[error("nats/jetstream error")]
    Nats(#[from] async_nats::Error),

    #[error("json decode error")]
    Json(#[from] serde_json::Error),

    #[error("tidb insert error")]
    Sqlx(#[from] sqlx::Error),
}

/// Consume `agent.trace.*` messages from a JetStream stream and persist them to TiDB.
///
/// Expectations:
/// - JetStream stream exists and is configured to capture `agent.trace.*` subjects.
/// - TiDB table exists: `agent_traces(agent_id, subject, payload_json, nats_sequence, received_at, ...)`
///
/// This function implements a simple at-least-once ingestion loop:
/// - insert into TiDB
/// - ACK the JetStream message after the insert succeeds
pub async fn ingest_agent_trace_stream(
    nats: async_nats::Client,
    tidb_pool: MySqlPool,
    stream_name: &str,
    durable_consumer_name: &str,
) -> Result<(), TraceIngestError> {
    let js = jetstream::new(nats);
    let stream = js
        .get_stream(stream_name)
        .await
        .map_err(|e| TraceIngestError::Nats(Box::new(e)))?;

    // Get-or-create a durable consumer filtered to `agent.trace.*`.
    // Using get_or_create_consumer is atomic: it avoids a race where a transient
    // get_consumer error incorrectly triggers creation of a duplicate consumer.
    let consumer = stream
        .get_or_create_consumer(
            durable_consumer_name,
            jetstream::consumer::pull::Config {
                durable_name: Some(durable_consumer_name.to_string()),
                filter_subject: "agent.trace.*".to_string(),
                ack_policy: jetstream::consumer::AckPolicy::Explicit,
                ..Default::default()
            },
        )
        .await
        .map_err(|e| TraceIngestError::Nats(Box::new(e)))?;

    loop {
        // Pull-based consumption with bounded buffering and natural backpressure.
        let mut batch = consumer
            .fetch()
            .max_messages(512)
            .expires(std::time::Duration::from_secs(1))
            .messages()
            .await
            .map_err(|e| TraceIngestError::Nats(Box::new(e)))?;

        while let Some(msg) = batch.next().await {
            let msg = msg.map_err(TraceIngestError::Nats)?;
            let subject = msg.subject.as_str().to_string();
            let agent_id = subject
                .split('.')
                .nth(2) // agent.trace.{agent_id}
                .unwrap_or("")
                .to_string();

            let payload: Value = serde_json::from_slice(&msg.payload)?;

            // Derive step_index from the payload if present.
            // Fallback: use the JetStream stream sequence number, which is guaranteed unique
            // per stream. This prevents two payload-less messages for the same agent from
            // colliding on the UNIQUE KEY (agent_id, step_index) and silently dropping data.
            let js_sequence = msg
                .info()
                .map(|i| i.stream_sequence as i64)
                .unwrap_or(0);
            let step_index = payload
                .get("step_index")
                .and_then(|v| v.as_i64())
                .unwrap_or(js_sequence) as i32;

            let event_type = payload
                .get("event_type")
                .and_then(|v| v.as_str())
                .map(|s| match s {
                    "tool_call" => AgentEventType::ToolCall,
                    "observation" => AgentEventType::Observation,
                    _ => AgentEventType::Thought,
                })
                .unwrap_or(AgentEventType::Thought);

            let trace = AgentTrace {
                agent_id,
                step_index,
                event_type,
                payload,
            };

            trace.insert(&tidb_pool).await?;

            msg.ack().await.map_err(TraceIngestError::Nats)?;
        }
    }
}

