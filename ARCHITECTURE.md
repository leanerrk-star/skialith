# Architecture

`durable_agent_core` is the durability layer for AI agent orchestration: **NATS JetStream as a write-ahead log** and **TiDB as the system of record**.

The guiding principle:

> **Make the event durable in the log first, then persist it to the database in efficient batches.**

This minimises end-to-end latency for the hot path while keeping TiDB write amplification under control.

## Validated performance

Benchmarks run against a local NATS + MySQL stack (Community Edition unless noted):

### Single-event latency

| Scenario | p50 | p95 | p99 |
|---|---|---|---|
| `save_event` (NATS PubAck) | 133 µs | 265 µs | 386 µs |
| Baseline MySQL INSERT | 986 µs | 1.5 ms | 2.6 ms |

NATS JetStream PubAck is **~7× faster** than a synchronous MySQL write — the core latency claim against DB-first alternatives (LangGraph, DBOS).

### Community vs Managed Edition under load

| Scenario | Community | Managed | Advantage |
|---|---|---|---|
| Backpressure flood (10 tasks × 2k events) | 19.4s / 1,028/sec | 0.36s / 55,039/sec | **54×** |
| 500 concurrent agents (10k events total) | 9.97s / 1,003/sec | 0.04s / 236,232/sec | **235×** |
| Batch persistence lag (1k events) | 91ms (batch=64, 100ms linger) | 8ms (batch=256, 10ms linger) | **11×** |

Community Edition enforces a 1,000 events/sec ceiling via backpressure. No events are lost — callers block until capacity is available. Managed removes the ceiling entirely.

---

## Components

### `DurableEventStore` — `src/durable_event_store.rs`

The core write path. Callers invoke `save_event(agent_id, event_id, payload)` which:

1. Blocks in the rate limiter until a slot is available in the current one-second window.
2. Serialises the envelope `{ agent_id, event_id, payload }` as JSON.
3. Publishes to JetStream subject `agent.trace.{agent_id}` and **awaits the PubAck** before returning — the WAL write is confirmed synchronous.
4. Enqueues the same payload to a background task via `tokio::mpsc` for batched TiDB persistence.

`DurableEventStore` is not `Clone`. Use `Arc<DurableEventStore>` for shared ownership. Cloning the `Arc` is cheap; the store itself holds a live NATS connection, a TiDB pool, and an instance lock that must not be duplicated.

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
| `channel_capacity` | 10,000 | 10,000 |

Config values that exceed Community caps are silently clamped in `new()`. Batch size and linger are compile-time gated (`--features managed`). The event rate limit is runtime-gated via the enterprise license key.

**JetStream stream verification**

`ensure_stream(js, stream_name, subjects)` calls `get_or_create_stream` at startup, making it safe to run against a fresh or already-provisioned NATS server without manual stream setup.

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

### Rate limiting and backpressure

The Community Edition enforces **1,000 events/sec** via backpressure — callers block until a slot opens in the current one-second window. Events are never dropped or rejected.

**Why backpressure, not rejection:**
Rejection means events are silently lost; the caller gets an error and moves on. Backpressure means the caller slows down but never loses data. Community Edition is lossless but bounded; the managed service is lossless and unbounded.

**Conversion trigger:** 50 concurrent production agents each firing ~3 events per step ≈ 1,000 events/sec. At that scale the slowdown is visible and the upgrade conversation is straightforward.

### Enterprise license key

The rate limit and HA rights are gated behind a **cryptographically signed license key**, not only the build flag. The `--features managed` flag alone is not sufficient to raise the limit.

```
DURABLE_LICENSE_KEY=<base64url(json_payload)>.<base64url(ed25519_signature)>
```

The JSON payload contains:
- `customer_id` — identifies the licensee
- `max_events_per_second` — `null` for unlimited; a number for a specific tier
- `allow_ha` — whether multi-instance deployment is permitted
- `expires_at` — Unix timestamp; expired licenses fall back to Community limits automatically

The signature is produced by Durable's private Ed25519 key, which never appears in this repository or any shipped binary. The corresponding public key is embedded in every build. Without a valid signature, `effective_rate_limit()` always returns `COMMUNITY_RATE_LIMIT` (1,000).

On startup, `connect()` calls `license::load()`, prints the edition and active limits to stderr, then passes the derived rate limit into the store.

### Single-instance enforcement — `src/instance_lock.rs`

Community Edition is not designed for high availability. Running multiple instances would:

- **Multiply the effective rate limit**: each instance has its own in-process rate limiter, so N instances = N × 1,000 events/sec, defeating the Community cap.
- **Create write races** in the TiDB batch writer, risking duplicate rows.
- **Split the dead-letter channel** across processes with no coordination.

On startup, Community Edition claims an atomic lock in NATS KV using `update(key, value, revision=0)` — NATS KV's CAS primitive that succeeds only if the key does not exist. A second instance finds the lock held and refuses to start with a clear error message.

**Lock lifecycle:**
- TTL: 30 seconds. Holder refreshes every 10 seconds.
- If the holder dies, the lock expires after at most 30 seconds — no automated failover.
- Recovery is manual: wait for TTL expiry, then start the replacement instance.

**Enterprise license with `allow_ha: true`** bypasses the lock entirely. The managed control plane provides a distributed rate limiter and coordinated multi-instance deployment.

### Data retention

Community Edition ships **no data purging or retention logic**. Rows in `agent_events` and `agent_traces` accumulate indefinitely. At production scale (millions of agent steps per month) this becomes a storage cost that motivates an upgrade.

The `RetentionPolicy` trait is declared in `src/managed/retention.rs`. The managed service implements:
- Configurable TTL per agent (e.g. retain last 30 days of traces)
- Automatic archival to cold storage before deletion
- Per-tier event count limits (e.g. keep last 10,000 steps per agent)
- A purge API for on-demand cleanup

### Open-core build split (`--features managed`)

The `managed` Cargo feature controls compile-time capabilities — batch size, flush interval, and which extension modules are compiled. It does **not** override the runtime license check.

| Capability | Community | `--features managed` (no license) | `--features managed` + valid license |
|---|---|---|---|
| Batch size | 64 | 256 | 256 |
| Flush interval | 100 ms | 10 ms | 10 ms |
| Events/sec | 1,000 (backpressure) | 1,000 (backpressure) | unlimited |
| HA / multi-instance | blocked | blocked | allowed |
| Data retention | none | none | configurable |

```
durable-agent-core  (this repo, public, BUSL)
    └── src/managed/replication.rs  — RegionReplicator trait (stub)
    └── src/managed/metrics.rs      — MetricsReporter trait (stub)
    └── src/managed/retention.rs    — RetentionPolicy trait (stub)

durable-agent-managed  (private repo)
    └── Cargo.toml: durable-agent-core = { features = ["managed"] }
    └── Concrete implementations of all managed traits
    └── Multi-region JetStream mirroring
    └── Prometheus / OpenTelemetry export
    └── Automated retention jobs
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
| `InstanceLock(String)` | Second Community Edition instance refused to start |

`save_event` no longer returns a rate-limit error. Callers that exceed the ceiling are blocked, not rejected.

---

## Observability

`tracing` instrumentation is active throughout:

- `debug!` on individual event enqueue and batch flush.
- `info!` on each successful TiDB batch commit with batch size.
- `warn!` on Community Edition limits being active, and on batch write retries.
- `eprintln!` for dead-letter events (deliberately not routed through `tracing` so they survive even if a subscriber is not configured).

---

## Environment variables

| Variable | Required | Default | Notes |
|---|---|---|---|
| `TIDB_URL` | yes | — | MySQL connection string |
| `NATS_URL` | no | `nats://127.0.0.1:4222` | |
| `SERVER_PORT` | no | `8080` | HTTP sidecar port |
| `DURABLE_LICENSE_KEY` | no | — | Enterprise license; unlocks rate limit and HA |

`.env` files are supported via `dotenvy`. `.gitignore` excludes `.env` and `.env.*`.

---

## Write path summary

```
Agent code
  │  save_event / checkpoint (HTTP POST)
  ▼
Axum sidecar (src/bin/server.rs)
  │  DurableEventStore::save_event
  │
  ├─► RateLimiter::wait()
  │     Community: block if > 1,000/sec  (backpressure, no loss)
  │     Managed:   no-op                 (immediate return)
  │
  ▼
NATS JetStream  ◄─── PubAck returned to caller ───
  │
  ├─► tokio::mpsc enqueue_tx
  │     └► Background batch writer
  │           └► TiDB: INSERT INTO agent_events (batched, retried, no TTL)
  │
  └─► JetStream consumer (trace_ingest)
        └► TiDB: INSERT INTO agent_traces (idempotent, per-message ACK, no TTL)
```

The caller unblocks as soon as JetStream acknowledges the write. TiDB persistence is asynchronous and best-effort with retries; failures route to the dead-letter channel rather than surfacing to the agent. Neither table has automated cleanup in Community Edition — data is retained indefinitely.
```
