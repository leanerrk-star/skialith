from dataclasses import dataclass
from typing import Any
import httpx

@dataclass
class AgentState:
    agent_id: str
    step_index: int
    data: dict[str, Any]   # the "state" field from the server response

class SkialithAgent:
    def __init__(self, agent_id: str, sidecar_url: str = "http://localhost:8080"):
        self.agent_id = agent_id
        self._url = sidecar_url.rstrip("/")
        self._client = httpx.AsyncClient(base_url=self._url, timeout=10.0)

    async def resume(self) -> AgentState:
        """Fetch the last checkpoint on startup. Returns a fresh state if none exists."""
        r = await self._client.get(f"/agents/{self.agent_id}/state")
        r.raise_for_status()
        body = r.json()
        return AgentState(
            agent_id=body["agent_id"],
            step_index=body["step_index"],
            data=body.get("state", {}),
        )

    async def checkpoint(self, step_index: int, state: dict[str, Any]) -> None:
        """Persist state before a risky operation. Blocks until the WAL confirms."""
        r = await self._client.post(
            f"/agents/{self.agent_id}/checkpoint",
            json={"step_index": step_index, "state": state},
        )
        r.raise_for_status()

    async def save_event(self, event_id: str, payload: dict[str, Any]) -> None:
        """Record an event to the trace log (observability / audit)."""
        r = await self._client.post(
            f"/agents/{self.agent_id}/events",
            json={"event_id": event_id, "payload": payload},
        )
        r.raise_for_status()

    async def close(self) -> None:
        await self._client.aclose()

    async def __aenter__(self):
        return self

    async def __aexit__(self, *_):
        await self.close()
