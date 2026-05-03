"""Shared API dependencies — breaks circular import between server and routes."""

from __future__ import annotations

from typing import Any

from ados.api.runtime import ApiRuntimeFacade, ensure_api_runtime

_agent_app: ApiRuntimeFacade | None = None


def set_agent_app(app: Any) -> None:
    global _agent_app
    _agent_app = ensure_api_runtime(app)


def get_agent_app() -> ApiRuntimeFacade:
    assert _agent_app is not None, "AgentApp not initialized"
    return _agent_app
