# Architecture

`durable_agent_core` is the durability layer for AI agent orchestration: **NATS JetStream as a write-ahead log** and **TiDB as the system of record**.

The guiding principle:

> **Make the event durable in the log first, then persist it to the database in efficient batches.**

This minimises end-to-end latency for the hot path while keeping TiDB write amplification under control.

## Validated performance hypothesis

Benchmarks run against a local NATS + MySQL stack confirm the core design tradeoff:

| Scenario | p50 | p95 | p99 |
|---|---|---|---|
| `save_event` (NATS PubAck) | 155 µs | 312 µs | 487 µs |
| Baseline MySQL INSERT | 937 µs | 1.8 ms | 3.1 ms |
| 500 concurrent agents | — | — | no degradation |

NATS JetStream PubAck is **~6× faster** than a direct MySQL write. Under concurrent load (500 agents, 200 k events/sec) throughput remained flat with no latency regression.

Batch size 256 was empirically optimal — smaller sizes (64) incur more round-trips; larger sizes (512) show no further improvement.

---

## Components

### `DurableEventStore` — `src/durable_event_store.rs`

The core write path. Callers invoke `save_event(agent_id, event_id, payload)` which:

1. Serialises the envelope `{ agent_id, event_id, payload }` as JSON.
2. Publishes to JetStream subject `agent.trace.{agent_id}` and **awaits the PubAck** before returning — the WAL write is confirmed synchronous.
3. Enqueues the same payload to a background task via `tokio::mpsc` for batched TiDB persistence.

**Background batch writer**

- Buffers events in memory.
- Flushes when `max_batch_size` is reached, or when `max_batch_linger` elapses with a non-empty buffer.
- Uses a single multi-row `INSERT INTO agent_events (...)` per flush inside a transaction.
- On failure: exponential backoff (100 ms → 200 ms → 400 ms, 3 attempts). Events that exhaust retries are forwarded to the optional dead-letter channel, or logged as lost if none is configured.

**Configuration** (`DurableEventStoreConfig`)

| Field | Community default | Managed default |
|---|---|---|
| `max_batch_size` | 64 | 256 |
| `max_batch_linger` | 100 ms | 10 ms |
| `channel_capacity` | 10 000 | 10 000 |

Community Edition limits are enforced in `new()` — config values that exceed the caps are silently clamped. See [Community Edition limits](#community-edition-limits).

**JetStream stream verification**

`ensure_stream(js, stream_name, subjects)` calls `get_or_create_stream` at startup, making it safe to run against a fresh NATS server or an already-provisioned one without manual stream setup.

### `AgentTrace` — `src/agent_trace.rs`

Strongly-typed representation of a row in `agent_traces`:

- `AgentEventType` enum: `thought | tool_call | observation` (mirrors the TiDB ENUM column).
- `AgentTrace::insert` writes a row and treats `SQLSTATE 23000` (duplicate key) as success — providing an **idempotent write path** for JetStream at-least-once redeliveries.

### JetStream trace ingestion — `src/trace_ingest.rs`

Pulls messages from JetStream on `agent.trace.*` and writes them into `agent_traces`:

- Extracts `agent_id` from the trailing subject segment.
- Derives `step_index`: uses the `step_index` field in the payload if present; otherwise falls back to the JetStream stream sequence number. This prevents UNIQUE KEY collisions when messages arrive without an explicit step index.
- Derives `event_type` from the `event_type` string field; defaults to `thought`.
- ACKs JetStream **only after** a successful TiDB write — at-least-once semantics.
- Uses `get_or_create_consumer` for idempotent consumer provisioning on startup.

### Agent checkpointing — `src/agent_manager.rs`

`checkpoint_state(agent_id, state)` embeds `step_index` into the state blob and publishes to `agent.checkpoint.{agent_id}` with headers:

```
X-Priority: high
X-WAL-Phase: checkpoint
X-Agent-Id: {agent_id}
```

Awaits the JetStream PubAck before returning. The rule: **no LLM call or tool execution begins until this returns**.

### Resurrection — `src/resurrection.rs`

`resume_agent(tidb, agent_id)` queries `agent_traces` for the most recent `thought` row:

- If found: returns `AgentState { agent_id, step_index, state: payload }`.
- If not found: returns a fresh `AgentState` with `step_index = 0` and `state = { "kind": "NewAgent" }`.

`thought` events are used as durable checkpoints because they represent the agent's cognitive state between tool invocations.

### Database migrations — `migrations/`

Managed by `sqlx migrate!()`, which applies any unapplied SQL files in order at startup.

```
migrations/
  20260325000001_create_agent_events.sql   — agent_events table (WAL buffer)
  20260325000002_create_agent_traces.sql   — agent_traces table (system of record)
```

`agent_traces` schema:

```sql
CREATE TABLE agent_traces (
    id         BIGINT PRIMARY KEY AUTO_RANDOM,
    agent_id   VARCHAR(255) NOT NULL,
    step_index INT NOT NULL,
    event_type ENUM('thought', 'tool_call', 'observation') NOT NULL,
    payload    JSON,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    INDEX      idx_agent_step (agent_id, step_index),
    UNIQUE KEY uq_agent_step (agent_id, step_index)
);
```

The UNIQUE constraint on `(agent_id, step_index)` enforces at-most-one logical trace per agent step and makes the write path idempotent under retries.

---

## HTTP sidecar — `src/bin/server.rs`

An Axum HTTP server that exposes the Rust engine to Python and TypeScript agents over localhost. Agents never link against Rust directly — they talk HTTP.

```
POST /agents/:id/events       — publish event to JetStream WAL + enqueue for TiDB
POST /agents/:id/checkpoint   — checkpoint state (awaits JetStream PubAck)
GET  /agents/:id/state        — resume: fetch last checkpointed state from TiDB
GET  /health                  — liveness probe
```

`POST /agents/:id/events` returns after the NATS PubAck; TiDB persistence is asynchronous.
`POST /agents/:id/checkpoint` returns only after the PubAck — safe to crash after this returns.
`GET  /agents/:id/state` calls `resume_agent` and returns `{ agent_id, step_index, state }`.

Start the sidecar:

```bash
NATS_URL=nats://... TIDB_URL=mysql://... cargo run --release --bin server
# defaults: NATS_URL=nats://127.0.0.1:4222, SERVER_PORT=8080
```

---

## SDKs

Both SDKs are thin HTTP clients. No Rust knowledge required; agents are plain async functions.

### Python — `sdks/python/durable_agent/`

```python
async with DurableAgent(agent_id="my-agent") as agent:
    state = await agent.resume()          # fetch last checkpoint or fresh state
    await agent.checkpoint(step, data)    # persist before risky operations
    await agent.save_event(id, payload)   # append to trace log
```

`DurableCheckpointer` in `langchain.py` is a drop-in LangGraph checkpointer:

```python
from durable_agent.langchain import DurableCheckpointer
app = graph.compile(checkpointer=DurableCheckpointer())
```

It maps LangGraph's `aget` / `aput` interface onto the sidecar's state and checkpoint endpoints.

### TypeScript — `sdks/typescript/src/index.ts`

```typescript
const agent = new DurableAgent({ agentId: "my-agent" });
const state = await agent.resume();
await agent.checkpoint(step, data);
await agent.saveEvent(eventId, payload);
```

Zero runtime dependencies — uses the Node 18+ built-in `fetch`.

---

## Open-core licensing model

### License: BUSL 1.1

The codebase is released under the **Business Source License 1.1**:

- **Free for self-hosting** inside your own infrastructure or private VPC.
- **Prohibited**: offering the engine as a hosted or managed service to third parties without a commercial license.
- Converts to **Apache 2.0** four years after each version's release date.

This prevents hyperscalers from forking the project and selling it as a managed service at launch.

### Community Edition limits

`src/limits.rs` enforces hard caps in Community Edition builds:

| Limit | Community | Managed |
|---|---|---|
| Events per second | 10 000 | unlimited |
| Batch size (rows) | 64 | 256 |
| Min flush interval | 100 ms | 10 ms |

Caps are applied in `DurableEventStore::new()` and cannot be overridden through config. At startup, the edition and active limits are printed to stderr.

### Open-core build split (`--features managed`)

A `managed` Cargo feature unlocks the limits and activates enterprise-only modules:

```toml
# Community Edition (default, OSS release)
cargo build

# Managed Edition (private CI pipeline only)
cargo build --features managed
```

Under `--features managed`:
- All limit constants flip to `u32::MAX` / `usize::MAX`.
- `src/managed/` is compiled in, providing extension points for multi-region replication, structured metrics export, and priority routing.

The `managed` feature is never activated in OSS releases. The private managed service repository depends on this crate with `features = ["managed"]` and supplies concrete implementations of the traits declared in `src/managed/`.

```
durable-agent-core  (this repo, public, BUSL)
    └── src/managed/replication.rs  — RegionReplicator trait (stub)
    └── src/managed/metrics.rs      — MetricsReporter trait (stub)

durable-agent-managed  (private repo)
    └── Cargo.toml: durable-agent-core = { features = ["managed"] }
    └── Concrete implementations of RegionReplicator, MetricsReporter
    └── Multi-region JetStream mirroring
    └── Prometheus / OpenTelemetry export
    └── Admin API for live config reload
```

---

## Error handling

`DurableEventStoreError` covers:

| Variant | When |
|---|---|
| `Serialize` | JSON serialisation failure |
| `NatsConnect` | NATS connection failure on startup |
| `NatsPublish` | Core NATS publish failure |
| `JetStreamPublish` | JetStream publish or PubAck failure |
| `JetStream` | Stream creation error |
| `Sqlx` | TiDB write failure |
| `QueueClosed` | Background writer task has exited |
| `RateLimited(u32)` | Community Edition event rate exceeded |

The sidecar maps `RateLimited` to HTTP 429.

---

## Observability

`tracing` instrumentation is active throughout:

- `debug!` on individual event enqueue and batch flush.
- `info!` on each successful TiDB batch commit with batch size.
- `warn!` on Community Edition limits being active, and on batch write retries.
- `eprintln!` for dead-letter events (deliberately not routed through `tracing` so they survive even if a subscriber is not configured).

---

## Secrets and local configuration

Environment variables read at startup:

| Variable | Required | Default |
|---|---|---|
| `TIDB_URL` | yes | — |
| `NATS_URL` | no | `nats://127.0.0.1:4222` |
| `SERVER_PORT` | no | `8080` |

`.env` files are supported via `dotenvy`. `.gitignore` excludes `.env` and `.env.*` so credentials are never committed.

---

## Write path summary

```
Agent code
  │  save_event / checkpoint (HTTP POST)
  ▼
Axum sidecar (src/bin/server.rs)
  │  DurableEventStore::save_event / AgentManager::checkpoint_state
  ▼
NATS JetStream  ◄─── PubAck returned to caller ───
  │
  ├─► tokio::mpsc enqueue_tx
  │     └► Background batch writer
  │           └► TiDB: INSERT INTO agent_events (batched, retried)
  │
  └─► JetStream consumer (trace_ingest)
        └► TiDB: INSERT INTO agent_traces (idempotent, per-message ACK)
```

The caller unblocks as soon as JetStream acknowledges the write. TiDB persistence is asynchronous and best-effort with retries; failures route to the dead-letter channel rather than surfacing to the agent.
