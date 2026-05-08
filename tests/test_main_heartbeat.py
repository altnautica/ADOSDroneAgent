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


def test_heartbeat_payload_video_restart_attempts_default() -> None:
    """No video pipeline attached → counter reads 0 (not absent)."""
    app = _fresh_app()
    payload = app._build_heartbeat_payload()
    assert payload["videoRestartAttempts"] == 0


def test_heartbeat_payload_video_restart_attempts_reflected() -> None:
    """A pipeline that exposes restart_attempts() shows up on the wire."""
    app = _fresh_app()

    class FakePipeline:
        def restart_attempts(self) -> int:
            return 3

    app._video_pipeline = FakePipeline()
    payload = app._build_heartbeat_payload()
    assert payload["videoRestartAttempts"] == 3
