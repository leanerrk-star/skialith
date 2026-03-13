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

### TiDB table: `agent_events`

The event store expects a table named `agent_events` with at least:

- `agent_id` (string)
- `event_id` (string)
- `payload_json` (JSON or text)
- optionally `created_at` default timestamp

Notes:

- The current implementation binds JSON as a string. TiDB’s JSON type accepts JSON text.
- For production use, consider indexes like `(agent_id, created_at)` and/or a unique constraint on `(agent_id, event_id)` if you want idempotent inserts.

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

- Add a migration for `agent_events` (and indexes/uniques based on query patterns).
- Add retry/backoff and/or a dead-letter path for failed TiDB writes.
- Consider publishing with JetStream acks and stream configuration verification for durability guarantees.
- Add tracing/metrics around queue depth, batch flush latency, and DB error rates.

