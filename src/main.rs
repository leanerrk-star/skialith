use durable_agent_core::durable_event_store::{DurableEventStore, DurableEventStoreConfig};
use serde::Serialize;

#[derive(Debug, Serialize)]
struct ExamplePayload {
    message: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Loads `.env` if present (and then falls back to real environment variables).
    let _ = dotenvy::dotenv();

    let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://127.0.0.1:4222".into());
    let tidb_url = std::env::var("TIDB_URL")?;

    let store = DurableEventStore::connect(&nats_url, &tidb_url, DurableEventStoreConfig::default())
        .await?;

    store
        .save_event(
            "agent-123",
            "event-001",
            &ExamplePayload {
                message: "hello from durable_event_store".to_string(),
            },
        )
        .await?;

    Ok(())
}
