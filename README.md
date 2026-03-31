# skialith

A durability layer for AI agents. Every event your agent produces is written to NATS JetStream as a write-ahead log before it touches the database — giving you sub-millisecond acknowledgement and crash recovery without building any of the plumbing yourself.

## Why

AI agents fail mid-run. When they do, you lose the work done so far and have to restart from scratch. `skialith` lets any agent checkpoint its state and replay from the last good step after a crash — whether the crash was in your code, the LLM API, or the host.

**Compared to checkpointing directly to a database:**

| | Database-first (LangGraph, DBOS) | `skialith` |
|---|---|---|
| Per-event write latency | ~1 ms (synchronous INSERT) | ~133 µs (NATS PubAck) |
| Under concurrent load | Single connection bottleneck | Parallel, batched to DB |
| Crash recovery | Possible | Yes, from last checkpoint |
| At-least-once delivery | Depends on implementation | Built-in (JetStream ACK) |

## Features

- **Fast event log** — publishes to NATS JetStream and returns to the caller as soon as the write-ahead log acknowledges. Database persistence happens in the background in batches.
- **Crash recovery** — `resume_agent` fetches the last checkpointed state on startup. Agents restart from where they left off, not from the beginning.
- **Idempotent writes** — duplicate events (from retries or redeliveries) are safely ignored at the database layer.
- **Python and TypeScript SDKs** — thin HTTP clients. No Rust knowledge required.
- **LangGraph drop-in** — `SkialithCheckpointer` implements the LangGraph checkpointer interface so existing graphs need no changes.
- **Self-hosted** — runs inside your own VPC. No data leaves your infrastructure.

## Quickstart

### 1. Start dependencies

```bash
docker compose up -d
```

This starts NATS (with JetStream) and MySQL on their default ports.

### 2. Run the sidecar

```bash
TIDB_URL=mysql://root:root@127.0.0.1:3306/skialith cargo run --release --bin server
```

The sidecar runs on port `8080` by default. Set `SERVER_PORT` to change it.

### 3. Use an SDK

**Python**

```python
from skialith import SkialithAgent

async with SkialithAgent(agent_id="my-agent") as agent:
    # On startup: resume from last checkpoint, or get a fresh state
    state = await agent.resume()

    # Before any risky operation: checkpoint current state
    await agent.checkpoint(step=state.step_index, data={"messages": messages})

    # After each step: append to the trace log
    await agent.save_event(event_id="step-1", payload={"kind": "thought", "text": "..."})
```

**TypeScript**

```typescript
import { SkialithAgent } from "./sdks/typescript/src";

const agent = new SkialithAgent({ agentId: "my-agent" });

const state = await agent.resume();
await agent.checkpoint(state.stepIndex, { messages });
await agent.saveEvent("step-1", { kind: "thought", text: "..." });
```

### 4. LangGraph integration

```python
from skialith.langchain import SkialithCheckpointer

checkpointer = SkialithCheckpointer()
app = graph.compile(checkpointer=checkpointer)

# Run as normal — checkpointing is handled transparently
result = await app.ainvoke({"messages": [...]}, config={"configurable": {"thread_id": "agent-1"}})
```

## HTTP API

The sidecar exposes four endpoints:

| Method | Path | Description |
|---|---|---|
| `POST` | `/agents/:id/events` | Append an event to the WAL. Returns after NATS acknowledges. |
| `POST` | `/agents/:id/checkpoint` | Persist state. Returns only after NATS acknowledges — safe to crash after this returns. |
| `GET` | `/agents/:id/state` | Fetch the last checkpoint, or a fresh state if none exists. |
| `GET` | `/health` | Liveness check. |

**POST /agents/:id/events**

```json
{ "event_id": "step-3", "payload": { "kind": "tool_call", "tool": "search" } }
```

Returns `204 No Content`. Database persistence is asynchronous.

**POST /agents/:id/checkpoint**

```json
{ "step_index": 3, "state": { "messages": [...] } }
```

Returns `204 No Content`. Safe to proceed with the next LLM call once this returns.

**GET /agents/:id/state**

```json
{ "agent_id": "my-agent", "step_index": 3, "state": { "messages": [...] } }
```

Returns `step_index: 0` and `state: { "kind": "NewAgent" }` when no checkpoint exists.

## Configuration

| Variable | Required | Default | Description |
|---|---|---|---|
| `TIDB_URL` | yes | — | MySQL-compatible connection string |
| `NATS_URL` | no | `nats://127.0.0.1:4222` | NATS server address |
| `SERVER_PORT` | no | `8080` | HTTP sidecar port |

`.env` files are loaded automatically via `dotenvy`.

## Rate limiting

The free edition imposes one limit: **1,000 events/sec**. When this ceiling is reached, callers block until capacity is available — no events are dropped or rejected.

See [LICENSE](./LICENSE) for the full terms.

## Running benchmarks

```bash
cargo run --bin benchmark
```

## License

[Business Source License 1.1](./LICENSE)

Free for self-hosting inside your own infrastructure or private VPC. Prohibited: offering this engine as a hosted or managed service to third parties without a commercial license. Converts to Apache 2.0 four years after each version's release date.

Contact: [hello@skialith.io](mailto:hello@skialith.io)
