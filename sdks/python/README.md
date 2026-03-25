# durable-agent-core Python SDK

Python client for the durable-agent-core HTTP sidecar — giving your AI agents crash-safe checkpointing and an audit trail out of the box.

## Installation

```bash
pip install durable-agent-core
```

For LangGraph support:

```bash
pip install 'durable-agent-core[langchain]'
```

## Quick Start

The core pattern is: **resume** on startup, **checkpoint** before risky operations, and let the sidecar handle crash recovery automatically.

```python
import asyncio
from durable_agent import DurableAgent

async def run_agent():
    async with DurableAgent(agent_id="my-agent-001") as agent:
        # 1. Resume from the last checkpoint (returns fresh state on first run)
        state = await agent.resume()
        print(f"Resuming from step {state.step_index}")

        # Step 0: gather inputs
        if state.step_index <= 0:
            inputs = {"query": "summarise Q1 results"}
            await agent.checkpoint(step_index=1, state={"inputs": inputs})
            await agent.save_event("inputs_gathered", {"query": inputs["query"]})
            state = await agent.resume()

        # Step 1: call an LLM (expensive / fallible)
        if state.step_index <= 1:
            result = await call_llm(state.data["inputs"]["query"])   # your LLM call
            await agent.checkpoint(step_index=2, state={**state.data, "result": result})
            await agent.save_event("llm_called", {"tokens": len(result)})
            state = await agent.resume()

        # Step 2: write output
        if state.step_index <= 2:
            await write_output(state.data["result"])                 # your output step
            await agent.save_event("output_written", {})

        print("Done!")

asyncio.run(run_agent())
```

### Crash-recovery pattern

If the process crashes between steps the sidecar retains the last checkpoint. On the next run `resume()` returns the saved `step_index`, so only the steps that had not yet been checkpointed are re-executed — no duplicate LLM calls, no lost work.

## LangGraph Integration

`DurableCheckpointer` is a drop-in replacement for LangGraph's built-in checkpointers:

```python
from langgraph.graph import StateGraph
from durable_agent.langchain import DurableCheckpointer

checkpointer = DurableCheckpointer(sidecar_url="http://localhost:8080")

builder = StateGraph(...)
# ... add nodes and edges ...
app = builder.compile(checkpointer=checkpointer)

# Run with a thread_id so the checkpointer can locate the right state
result = await app.ainvoke(
    {"messages": [...]},
    config={"configurable": {"thread_id": "conversation-42"}},
)
```

Every `aput` call persists the LangGraph checkpoint to durable-agent-core. On restart, `aget` restores the last saved checkpoint so the graph continues from where it left off.

## Running the Sidecar

```bash
docker run -p 8080:8080 \
  -e NATS_URL=nats://your-nats-host:4222 \
  -e TIDB_URL=mysql://user:pass@your-tidb-host:4000/dbname \
  durable/agent-core
```

The sidecar exposes:

| Endpoint | Purpose |
|---|---|
| `POST /agents/{id}/events` | Append an event to the trace log |
| `POST /agents/{id}/checkpoint` | Persist a checkpoint |
| `GET /agents/{id}/state` | Fetch the latest checkpoint |
| `GET /health` | Liveness probe |
