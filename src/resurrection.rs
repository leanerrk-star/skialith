use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{types::Json, MySqlPool};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ResurrectionError {
    #[error("tidb query failed")]
    Sqlx(#[from] sqlx::Error),
}

/// State returned by `resume_agent`.
///
/// This is intentionally JSON-centric so it can represent arbitrary agent state without
/// forcing a schema migration for Rust structs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentState {
    pub agent_id: String,
    pub step_index: i64,
    pub state: Value,
}

impl AgentState {
    pub fn new_agent(agent_id: String) -> Self {
        Self {
            agent_id,
            step_index: 0,
            state: json!({
                "kind": "NewAgent",
                "step_index": 0
            }),
        }
    }
}

/// Resurrection logic: if the worker crashes, resume from TiDB using the last checkpoint.
///
/// Expected schema (aligned with `agent_traces`):
/// ```sql
/// CREATE TABLE agent_traces (
///     id BIGINT PRIMARY KEY AUTO_RANDOM,
///     agent_id VARCHAR(255) NOT NULL,
///     step_index INT NOT NULL,
///     event_type ENUM('thought', 'tool_call', 'observation') NOT NULL,
///     payload JSON,
///     created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
///     INDEX idx_agent_step (agent_id, step_index)
/// );
/// ```
///
/// Behavior:
/// - Fetch the row with the highest `step_index` for the given `agent_id`
///   (optionally constrained by `event_type` semantics, here we assume `thought`
///   rows carry the checkpointed state).
/// - Return its JSON `payload` as the agent state.
/// - If no row exists, return a default "New Agent" state.
pub async fn resume_agent(
    tidb_pool: &MySqlPool,
    agent_id: String,
) -> Result<AgentState, ResurrectionError> {
    let row: Option<(i64, Option<Json<Value>>)> = sqlx::query_as(
        r#"
        SELECT step_index, payload
        FROM agent_traces
        WHERE agent_id = ? AND event_type = 'thought'
        ORDER BY step_index DESC
        LIMIT 1
        "#,
    )
    .bind(&agent_id)
    .fetch_optional(tidb_pool)
    .await?;

    Ok(match row {
        Some((step_index, maybe_payload)) => {
            let state = maybe_payload
                .map(|j| j.0)
                .unwrap_or_else(|| json!({"kind": "Unknown", "step_index": step_index}));

            AgentState {
                agent_id,
                step_index,
                state,
            }
        }
        None => AgentState::new_agent(agent_id),
    })
}

