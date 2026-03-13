# Architecture

This crate (`durable_agent_core`) is the beginning of an AI-native agent orchestrator’s durability layer: **NATS (JetStream) for a write-ahead event log** and **TiDB X as the system of record**.

The guiding principle is:

> **Make the event visible/durable in the log first, then persist it in the database in efficient batches.**

This minimizes end-to-end latency for the “trace” stream while keeping TiDB write amplification under control.

## Components

### `DurableEventStore`

Source: `src/durable_event_store.rs`

Responsibilities:

- **Publish an event to NATS** on subject `agent.{agent_id}.trace`.
- **Batch-write events to TiDB** into the `agent_events` table using `sqlx`.
- Provide an **async API** via `save_event(...)` that fits idiomatic Rust async/await.

Key behavior:

- `save_event(...)`:
  - Serializes an envelope `{ agent_id, event_id, payload }` as JSON.
  - Publishes the JSON to NATS subject `agent.{agent_id}.trace` (the “WAL” step).
  - Enqueues the same payload to a background task for batched TiDB persistence.

- Background batch writer:
  - Buffers events in-memory.
  - Flushes when either:
    - `max_batch_size` is reached, or
    - `max_batch_linger` elapses since the last tick with a non-empty batch.
  - Uses a single multi-row `INSERT INTO agent_events (...) VALUES (...), (...), ...` for throughput.
  - Writes within a transaction for atomicity of each batch insert.

Why this design:

- **NATS first**: publishing to NATS is the lowest-latency path to make the event available to downstream consumers (and to JetStream, if the subject is captured by a configured stream).
- **TiDB batched writes**: batching reduces per-event overhead (round trips, transaction costs) and improves throughput.
- **Async boundary via channel**: `tokio::mpsc` provides backpressure and isolates the caller from the DB’s performance profile.

### TiDB tables

#### `agent_events` (DurableEventStore)

`DurableEventStore` currently targets a generic `agent_events` table for its batched inserts:

- `agent_id` (string)
- `event_id` (string)
- `payload_json` (JSON or text)
- optionally `created_at` default timestamp

Notes:

- The implementation binds JSON as UTF‑8 text; TiDB’s `JSON` type happily accepts this.
- For production use, consider indexes like `(agent_id, created_at)` and/or a unique constraint on `(agent_id, event_id)` if you want idempotent inserts.

#### `agent_traces` (canonical trace/event log)

The system-of-record schema for traces is:

```sql
CREATE TABLE agent_traces (
    id BIGINT PRIMARY KEY AUTO_RANDOM,
    agent_id VARCHAR(255) NOT NULL,
    step_index INT NOT NULL,
    event_type ENUM('thought', 'tool_call', 'observation') NOT NULL,
    payload JSON,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    INDEX idx_agent_step (agent_id, step_index)
);
```

This table is populated by the JetStream ingester (see below) using the strongly-typed `AgentTrace` struct.

### `AgentTrace` and event typing

Source: `src/agent_trace.rs`

- **`AgentEventType`**: Rust enum mirroring the TiDB `event_type` ENUM (`thought`, `tool_call`, `observation`).
- **`AgentTrace`**: in-memory representation of a row in `agent_traces`:
  - `agent_id: String`
  - `step_index: i32`
  - `event_type: AgentEventType`
  - `payload: serde_json::Value`
- **`AgentTrace::insert`**: helper that performs:

  ```sql
  INSERT INTO agent_traces (agent_id, step_index, event_type, payload) VALUES (?, ?, ?, ?)
  ```

Why:

- Centralizes how trace rows are written so both ingestion and any future writers stay schema-aligned.
- Encodes the event type in a first-class enum instead of stringly-typed literals.

### JetStream trace ingestion (`trace_ingest`)

Source: `src/trace_ingest.rs`

Responsibilities:

- Consume messages from JetStream for subject pattern `agent.trace.*`.
- Decode the JSON payload with `serde_json`.
- Derive `step_index` and `event_type` from the payload (with sensible defaults).
- Persist into TiDB’s `agent_traces` using `AgentTrace::insert`.
- ACK JetStream messages **only after** a successful DB write (at-least-once semantics).

Behavior:

- Subject parsing:
  - Extracts `agent_id` from `agent.trace.{agent_id}`.
- Payload expectations:
  - If `step_index` exists as an integer, it is used; otherwise defaults to `0`.
  - If `event_type` is `"tool_call"` or `"observation"`, those variants are used; anything else defaults to `"thought"`.
- Backpressure:
  - Uses a pull consumer (`fetch().max_messages(...).expires(...)`) and processes messages in bounded batches.

Why this design:

- Gives you a clean, typed bridge from JetStream to TiDB aligned with the `agent_traces` schema.
- Decouples NATS subject shape and JSON envelope details from the database layout.

### Agent checkpointing (`AgentManager`)

Source: `src/agent_manager.rs`

Responsibilities:

- Provide a simple API for agents to **checkpoint state before calling an LLM or tools**.
- Ensure the **WAL is updated first** by publishing into JetStream and awaiting the PubAck.

Key behavior:

- `checkpoint_state(agent_id, state)`:
  - Serializes the arbitrary JSON-compatible `state`.
  - Publishes to JetStream subject `agent.checkpoint.{agent_id}`.
  - Attaches headers:
    - `X-Priority: high`
    - `X-WAL-Phase: checkpoint`
    - `X-Agent-Id: {agent_id}`
  - Awaits the JetStream PubAck future and only then returns to the caller.

Why:

- Encodes the “WAL before work” rule: no tool execution should happen until this high-priority checkpoint has been durably appended.
- Uses headers to carry priority/phase information in a way that downstream executors and observability tooling can inspect.

### Resurrection logic (`resurrection`)

Source: `src/resurrection.rs`

Responsibilities:

- Provide **“resurrection”** semantics when a worker crashes: recover the last known state for an agent from TiDB and resume.

Key behavior:

- `resume_agent(agent_id)`:
  - Queries `agent_traces` for the latest `step_index` for that `agent_id` **where `event_type = 'thought'`**.
  - Interprets the `payload` JSON for that row as the authoritative checkpointed state.
  - If a row exists:
    - Returns `AgentState { agent_id, step_index, state: payload }`.
  - If no row exists:
    - Returns a default `New Agent` state:
      - `step_index = 0`
      - `state = { "kind": "NewAgent", "step_index": 0 }`.

Why:

- Treats `thought` events as durable checkpoints of the agent’s cognitive state between tool invocations.
- Aligns with the indexed `(agent_id, step_index)` pattern for efficient “latest state” lookups.

## Configuration

`DurableEventStoreConfig` controls batching:

- `max_batch_size`: upper bound of events per insert.
- `max_batch_linger`: time-based flush to prevent “small trickle” events from waiting too long.
- `channel_capacity`: queue depth between ingestion and DB writer (controls memory vs backpressure).

Rationale:

- Size-based flushing maximizes throughput under load.
- Time-based flushing bounds latency under low volume.
- Channel capacity prevents unbounded memory growth; if the queue fills, callers will naturally experience backpressure.

## Error handling

We use `thiserror` to provide a structured error type:

- JSON serialization failures
- NATS connect/publish failures (via `async-nats`)
- SQL write failures (via `sqlx`)
- Queue closure (e.g., background task exiting)

Why:

- This keeps the public API ergonomic (`Result<(), DurableEventStoreError>`) and makes failure modes explicit and testable.

## Dependencies (why they were chosen)

`Cargo.toml` includes:

- `tokio`: async runtime.
- `async-nats`: high-performance async NATS client suitable for JetStream-backed usage.
- `sqlx` (mysql + json + tokio-rustls): async MySQL/TiDB connectivity with compile-time checked APIs (when used with macros; we currently use `QueryBuilder`).
- `serde` / `serde_json`: stable JSON encoding for event payloads.
- `dotenvy`: local/dev credential management without committing secrets.
- `thiserror`: idiomatic error composition.

## Secrets & local configuration

We intentionally **did not hardcode** the TiDB connection string (it contains credentials).

Instead:

- `src/main.rs` reads:
  - `TIDB_URL` (required)
  - `NATS_URL` (defaults to `nats://127.0.0.1:4222`)
- `.gitignore` ignores `.env` and `.env.*` so secrets are not committed, while allowing `.env.example` for documentation.

Why:

- Keeps credentials out of the repo and out of source history.
- Works well with local development and CI/CD secret injection.

## Operational notes / expected semantics

- This is a **WAL + async persistence** architecture. Without additional coordination, the system is typically **at-least-once** end-to-end:
  - NATS publish may succeed while TiDB insert later fails (event exists in log but not in DB yet).
  - If you add retries for TiDB inserts, you may need **idempotency** at the DB layer (unique key on `(agent_id, event_id)`).

- Ordering:
  - Per `agent_id`, NATS subjecting naturally groups trace events, but strict ordering depends on publishers and consumers.
  - TiDB inserts are batched and may not reflect exact publish time ordering unless `created_at` is generated from publish timestamps.

## What to do next

Common next steps for hardening:

- Add migrations for both `agent_events` (if kept) and `agent_traces` that match how the code reads/writes.
- Add retry/backoff and/or a dead-letter path for failed TiDB writes.
- Consider publishing with JetStream acks and stream configuration verification for durability guarantees.
- Add tracing/metrics around queue depth, batch flush latency, and DB error rates.

