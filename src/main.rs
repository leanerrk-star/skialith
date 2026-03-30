use skialith::durable_event_store::{SkialithStore, SkialithConfig};
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

    let store = SkialithStore::connect(&nats_url, &tidb_url, SkialithConfig::default())
        .await?;

    sqlx::migrate!().run(store.tidb_pool()).await?;

    store
        .save_event(
            "agent-123",
            "event-001",
            &ExamplePayload {
                message: "hello from skialith_store".to_string(),
            },
        )
        .await?;

    Ok(())
}
