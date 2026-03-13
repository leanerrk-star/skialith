use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{types::Json, MySqlPool};

/// Event type for `agent_traces.event_type` enum.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentEventType {
    Thought,
    ToolCall,
    Observation,
}

impl AgentEventType {
    pub fn as_db_str(self) -> &'static str {
        match self {
            AgentEventType::Thought => "thought",
            AgentEventType::ToolCall => "tool_call",
            AgentEventType::Observation => "observation",
        }
    }
}

/// In-memory representation of a row in `agent_traces`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTrace {
    pub agent_id: String,
    pub step_index: i32,
    pub event_type: AgentEventType,
    pub payload: Value,
}

impl AgentTrace {
    /// Persist this trace into TiDB using the `agent_traces` schema.
    pub async fn insert(&self, pool: &MySqlPool) -> Result<(), sqlx::Error> {
        let res = sqlx::query(
            r#"
            INSERT INTO agent_traces (agent_id, step_index, event_type, payload)
            VALUES (?, ?, ?, ?)
            "#,
        )
        .bind(&self.agent_id)
        .bind(self.step_index)
        .bind(self.event_type.as_db_str())
        .bind(Json(&self.payload))
        .execute(pool)
        .await;

        match res {
            Ok(_) => Ok(()),
            Err(e) => {
                // Idempotency layer:
                // If a UNIQUE constraint exists on (agent_id, step_index) and this insert races
                // with another writer, MySQL/TiDB will raise a duplicate key error.
                // We treat that case as "already persisted" and do not surface an error.
                if let Some(db_err) = e.as_database_error() {
                    let code = db_err.code().unwrap_or_default();
                    // MySQL/TiDB use SQLSTATE 23000 and vendor code 1062 for duplicate keys.
                    if code == "23000" && db_err.message().contains("Duplicate") {
                        return Ok(());
                    }
                }

                Err(e)
            }
        }

    }
}

