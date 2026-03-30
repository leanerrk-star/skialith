# @skialith/agent-core

Skialith execution layer for AI agents — a TypeScript/Node.js SDK that wraps the Skialith HTTP sidecar.

## Installation

```bash
npm install @skialith/agent-core
```

**Requirement:** Node.js 18+ (uses the built-in `fetch` API — zero runtime dependencies)

## Quick Start

```typescript
import { SkialithAgent } from '@skialith/agent-core';

const agent = new SkialithAgent({ agentId: 'research-agent-42' });

async function runAgent(task: string): Promise<string> {
  // On startup: load last checkpoint, or fresh state if new
  const state = await agent.resume();

  if (state.stepIndex < 1) {
    const thought = await llm.invoke(task);
    await agent.checkpoint(1, { thought });
  }

  if (state.stepIndex < 2) {
    const result = await browserTool.run(state.data.thought as string);
    await agent.checkpoint(2, { ...state.data, result });
  }

  if (state.stepIndex < 3) {
    const answer = await llm.synthesize(state.data.result as string);
    await agent.checkpoint(3, { ...state.data, answer });
  }

  return state.data.answer as string;
}
```

### Crash Recovery Pattern

The pattern above is intentional: each step checks `state.stepIndex < N` before running. If the process crashes mid-run, calling `resume()` on restart loads the last committed checkpoint, and execution picks up from where it left off — skipping all steps that already completed.

## API

### `new SkialithAgent(options)`

| Option | Type | Default | Description |
|---|---|---|---|
| `agentId` | `string` | required | Unique identifier for this agent instance |
| `sidecarUrl` | `string` | `http://localhost:8080` | URL of the Skialith sidecar |

### `agent.resume(): Promise<AgentState>`

Fetches the last persisted checkpoint. Returns `{ agentId, stepIndex: 0, data: {} }` if no checkpoint exists yet. Call this at the start of every agent run.

### `agent.checkpoint(stepIndex, state): Promise<void>`

Persists agent state durably. Blocks until the write-ahead log confirms the write — safe to crash after this returns.

### `agent.saveEvent(eventId, payload): Promise<void>`

Records an event to the trace log for observability and audit. Returns after the NATS PubAck; TiDB persistence is asynchronous.

## Running the Sidecar

```bash
docker run -p 8080:8080 \
  -e NATS_URL=nats://your-nats-host:4222 \
  -e TIDB_URL=mysql://user:pass@your-tidb-host:4000/dbname \
  skialith/agent-core
```

## Framework Compatibility

Works with any Node.js AI framework. For LangChain.js, call `resume()` at the start of your graph and `checkpoint()` before each node that performs a risky operation (LLM call, tool execution, external API call).
