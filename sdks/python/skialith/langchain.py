"""LangGraph checkpointer backed by skialith."""
from typing import Any, AsyncIterator, Iterator, Optional, Sequence, Tuple
from langchain_core.runnables import RunnableConfig

try:
    from langgraph.checkpoint.base import (
        BaseCheckpointSaver,
        Checkpoint,
        CheckpointMetadata,
        CheckpointTuple,
    )
    _LANGGRAPH_AVAILABLE = True
except ImportError:
    _LANGGRAPH_AVAILABLE = False

import httpx


def _require_langgraph():
    if not _LANGGRAPH_AVAILABLE:
        raise ImportError(
            "langgraph is required: pip install 'skialith[langchain]'"
        )


class SkialithCheckpointer:
    """Drop-in LangGraph checkpointer that persists state via skialith.

    Usage:
        from skialith.langchain import SkialithCheckpointer
        checkpointer = SkialithCheckpointer(sidecar_url="http://localhost:8080")
        app = graph.compile(checkpointer=checkpointer)
    """

    def __init__(self, sidecar_url: str = "http://localhost:8080"):
        _require_langgraph()
        self._url = sidecar_url.rstrip("/")
        self._client = httpx.AsyncClient(base_url=self._url, timeout=10.0)

    def _thread_id(self, config: RunnableConfig) -> str:
        return config["configurable"].get("thread_id", "default")

    async def aget(self, config: RunnableConfig) -> Optional[CheckpointTuple]:
        thread_id = self._thread_id(config)
        r = await self._client.get(f"/agents/{thread_id}/state")
        if r.status_code == 404:
            return None
        r.raise_for_status()
        body = r.json()
        state = body.get("state", {})
        if state.get("kind") == "NewAgent":
            return None
        checkpoint = state.get("checkpoint")
        metadata = state.get("metadata", {})
        if checkpoint is None:
            return None
        return CheckpointTuple(
            config=config,
            checkpoint=checkpoint,
            metadata=metadata,
        )

    async def aput(
        self,
        config: RunnableConfig,
        checkpoint: Checkpoint,
        metadata: CheckpointMetadata,
        new_versions: Any,
    ) -> RunnableConfig:
        thread_id = self._thread_id(config)
        step = metadata.get("step", 0)
        await self._client.post(
            f"/agents/{thread_id}/checkpoint",
            json={
                "step_index": step,
                "state": {"checkpoint": checkpoint, "metadata": metadata},
            },
        )
        return config
