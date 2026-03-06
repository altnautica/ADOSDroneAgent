"""Shared API dependencies — breaks circular import between server and routes."""

from __future__ import annotations

from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from ados.core.main import AgentApp

_agent_app: Any = None


def set_agent_app(app: AgentApp) -> None:
    global _agent_app
    _agent_app = app


def get_agent_app() -> AgentApp:
    assert _agent_app is not None, "AgentApp not initialized"
    return _agent_app
