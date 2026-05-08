"""Direct tests for AgentApp._build_heartbeat_payload.

The cloud subprocess in `services/cloud/__main__.py` is the production
heartbeat path; this module exercises the parallel single-process
builder in `core/main.py` so the contract stays in lockstep across
both surfaces.
"""

from __future__ import annotations

from ados.core.config import ADOSConfig
from ados.core.main import AgentApp


def _fresh_app() -> AgentApp:
    """Build an AgentApp without running .start() (no asyncio loop)."""
    config = ADOSConfig()
    app = AgentApp(config)
    return app


def test_heartbeat_payload_emits_runtime_mode_full() -> None:
    app = _fresh_app()
    payload = app._build_heartbeat_payload()
    assert payload["runtimeMode"] == "full"
