CREATE TABLE IF NOT EXISTS agent_traces (
    id BIGINT PRIMARY KEY AUTO_RANDOM,
    agent_id VARCHAR(255) NOT NULL,
    step_index INT NOT NULL,
    event_type ENUM('thought', 'tool_call', 'observation') NOT NULL,
    payload JSON,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    INDEX idx_agent_step (agent_id, step_index),
    UNIQUE KEY uq_agent_step (agent_id, step_index)
);
