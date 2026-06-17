"""Tests for the PIC events WebSocket backpressure handling.

The /pic/events WS used to allocate an unbounded asyncio.Queue, so a
slow client (e.g. on a degraded cellular link) could grow the queue
without limit. The QueueFull branch was dead code. We now bound the
queue and drop the oldest event when it fills, keeping the most recent
state visible to the client during sustained backpressure.
"""

from __future__ import annotations

import pytest
from fastapi.testclient import TestClient

from ados.api.server import create_app
from ados.core.config import ADOSConfig
from tests.api_runtime_utils import build_api_runtime


@pytest.fixture
def client(tmp_path, monkeypatch):
    monkeypatch.setenv("ADOS_PROFILE_OVERRIDE", "ground_station")

    cfg = ADOSConfig()
    cfg.agent.profile = "ground_station"
    app = build_api_runtime(config=cfg, uptime_seconds=0.0)
    return TestClient(create_app(app))


def test_pic_ws_no_unbounded_asyncio_queue_in_routes():
    """A safety net: no other route file should reintroduce an unbounded
    asyncio.Queue() (default maxsize=0). Look for the literal pattern."""
    from pathlib import Path

    routes_dir = Path(__file__).resolve().parent.parent / "src" / "ados" / "api" / "routes"
    offenders: list[str] = []
    for py in routes_dir.rglob("*.py"):
        text = py.read_text(encoding="utf-8", errors="replace")
        # asyncio.Queue() with no args = unbounded. Allow asyncio.Queue(maxsize=...)
        # Crude grep, sufficient for a guard.
        for line in text.splitlines():
            stripped = line.strip()
            if "asyncio.Queue()" in stripped and "maxsize" not in stripped:
                offenders.append(f"{py.relative_to(routes_dir)}: {stripped}")
    assert not offenders, "unbounded asyncio.Queue in routes: " + "; ".join(offenders)
