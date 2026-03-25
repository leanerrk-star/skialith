CREATE TABLE IF NOT EXISTS agent_events (
    id BIGINT PRIMARY KEY AUTO_RANDOM,
    agent_id VARCHAR(255) NOT NULL,
    event_id VARCHAR(255) NOT NULL,
    payload_json JSON,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    INDEX idx_agent_events_agent_id (agent_id),
    UNIQUE KEY uq_agent_event (agent_id, event_id)
);
