CREATE TABLE IF NOT EXISTS agent_events (
    id          BIGINT PRIMARY KEY AUTO_INCREMENT,
    agent_id    VARCHAR(255) NOT NULL,
    event_id    VARCHAR(255) NOT NULL,
    payload_json JSON,
    created_at  TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    INDEX idx_agent_events_agent_id (agent_id),
    UNIQUE KEY uq_agent_event (agent_id, event_id)
);

CREATE TABLE IF NOT EXISTS agent_traces (
    id          BIGINT PRIMARY KEY AUTO_INCREMENT,
    agent_id    VARCHAR(255) NOT NULL,
    step_index  INT NOT NULL,
    event_type  ENUM('thought', 'tool_call', 'observation') NOT NULL,
    payload     JSON,
    created_at  TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    INDEX idx_agent_step (agent_id, step_index),
    UNIQUE KEY uq_agent_step (agent_id, step_index)
);
