use async_nats::jetstream;
use futures::StreamExt;
use serde_json::Value;
use sqlx::{types::Json, MySqlPool};
use thiserror::Error;

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

    // Create-or-reuse a durable consumer filtered to `agent.trace.*`.
    let consumer = match stream.get_consumer(durable_consumer_name).await {
        Ok(c) => c,
        Err(_) => {
            let cfg = jetstream::consumer::pull::Config {
                durable_name: Some(durable_consumer_name.to_string()),
                filter_subject: "agent.trace.*".to_string(),
                ack_policy: jetstream::consumer::AckPolicy::Explicit,
                ..Default::default()
            };
            stream
                .create_consumer(cfg)
                .await
                .map_err(|e| TraceIngestError::Nats(Box::new(e)))?
        }
    };

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

            // Uses the provided pool (sqlx will multiplex connections).
            sqlx::query(
                r#"
                INSERT INTO agent_traces (agent_id, subject, payload_json)
                VALUES (?, ?, ?)
                "#,
            )
            .bind(agent_id)
            .bind(subject)
            .bind(Json(payload))
            .execute(&tidb_pool)
            .await?;

            msg.ack().await.map_err(TraceIngestError::Nats)?;
        }
    }
}

