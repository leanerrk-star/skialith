/// HTTP sidecar server for durable_agent_core.
///
/// Exposes three endpoints that Python and TypeScript SDKs call:
///
///   POST /agents/:id/events      — record an event to the WAL + TiDB
///   POST /agents/:id/checkpoint  — checkpoint state before a risky operation
///   GET  /agents/:id/state       — resume: fetch the last checkpoint on startup
///
/// Run:
///   NATS_URL=nats://... TIDB_URL=mysql://... cargo run --release --bin server
///   # defaults: NATS_URL=nats://127.0.0.1:4222, SERVER_PORT=8080
use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
    routing::{get, post},
    Router,
};
use durable_agent_core::{
    agent_manager::AgentManager,
    durable_event_store::{DurableEventStore, DurableEventStoreConfig},
    resurrection::resume_agent,
};
use serde::{Deserialize, Serialize};

// ── shared state ─────────────────────────────────────────────────────────────

#[derive(Clone)]
struct AppState {
    store: Arc<DurableEventStore>,
    manager: Arc<AgentManager>,
    tidb: sqlx::MySqlPool,
}

// ── request / response types ─────────────────────────────────────────────────

#[derive(Deserialize)]
struct SaveEventRequest {
    event_id: String,
    payload: serde_json::Value,
}

#[derive(Deserialize)]
struct CheckpointRequest {
    step_index: i32,
    state: serde_json::Value,
}

#[derive(Serialize)]
struct AgentStateResponse {
    agent_id: String,
    step_index: i64,
    state: serde_json::Value,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

type ApiError = (StatusCode, Json<ErrorResponse>);

fn internal(e: impl ToString) -> ApiError {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse { error: e.to_string() }),
    )
}

// ── handlers ─────────────────────────────────────────────────────────────────

/// POST /agents/:agent_id/events
///
/// Records an event to the NATS WAL and enqueues it for TiDB persistence.
/// Returns immediately after the NATS PubAck — TiDB write is async.
async fn save_event(
    Path(agent_id): Path<String>,
    State(app): State<AppState>,
    Json(body): Json<SaveEventRequest>,
) -> Result<StatusCode, ApiError> {
    app.store
        .save_event(&agent_id, &body.event_id, &body.payload)
        .await
        .map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

/// POST /agents/:agent_id/checkpoint
///
/// Persists agent state to JetStream and awaits the PubAck before returning.
/// Call this before any LLM call or tool execution so a crash can be recovered.
async fn checkpoint(
    Path(agent_id): Path<String>,
    State(app): State<AppState>,
    Json(body): Json<CheckpointRequest>,
) -> Result<StatusCode, ApiError> {
    // Embed step_index inside the state blob so resurrection can derive it.
    let payload = serde_json::json!({
        "step_index": body.step_index,
        "state": body.state,
    });
    app.manager
        .checkpoint_state(&agent_id, &payload)
        .await
        .map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

/// GET /agents/:agent_id/state
///
/// Returns the last checkpointed state for this agent.
/// If no checkpoint exists, returns step_index=0 and a "NewAgent" state.
/// Call this on agent startup to resume after a crash.
async fn get_state(
    Path(agent_id): Path<String>,
    State(app): State<AppState>,
) -> Result<Json<AgentStateResponse>, ApiError> {
    let agent_state = resume_agent(&app.tidb, agent_id)
        .await
        .map_err(internal)?;

    Ok(Json(AgentStateResponse {
        agent_id: agent_state.agent_id,
        step_index: agent_state.step_index,
        state: agent_state.state,
    }))
}

/// GET /health
///
/// Liveness check. Returns 200 when the server is ready to accept requests.
async fn health() -> StatusCode {
    StatusCode::OK
}

// ── main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();

    let nats_url =
        std::env::var("NATS_URL").unwrap_or_else(|_| "nats://127.0.0.1:4222".into());
    let tidb_url = std::env::var("TIDB_URL")
        .expect("TIDB_URL environment variable is required");
    let port = std::env::var("SERVER_PORT")
        .unwrap_or_else(|_| "8080".into())
        .parse::<u16>()
        .expect("SERVER_PORT must be a valid port number");

    let store = Arc::new(
        DurableEventStore::connect(&nats_url, &tidb_url, DurableEventStoreConfig::default())
            .await?,
    );

    let nats_client = async_nats::connect(&nats_url).await?;
    let manager = Arc::new(AgentManager::new(nats_client));
    let tidb = store.tidb_pool().clone();

    let state = AppState { store, manager, tidb };

    let app = Router::new()
        .route("/health", get(health))
        .route("/agents/{agent_id}/events", post(save_event))
        .route("/agents/{agent_id}/checkpoint", post(checkpoint))
        .route("/agents/{agent_id}/state", get(get_state))
        .with_state(state);

    let addr = format!("0.0.0.0:{port}");
    println!("durable-agent-core server listening on {addr}");

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
