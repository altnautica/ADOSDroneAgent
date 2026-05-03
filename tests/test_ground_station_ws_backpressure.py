"""Tests for the PIC events WebSocket backpressure handling.

The /pic/events WS used to allocate an unbounded asyncio.Queue, so a
slow client (e.g. on a degraded cellular link) could grow the queue
without limit. The QueueFull branch was dead code. We now bound the
queue and drop the oldest event when it fills, keeping the most recent
state visible to the client during sustained backpressure.
"""

from __future__ import annotations

import time
from unittest.mock import MagicMock

import pytest
from fastapi.testclient import TestClient

from ados.api.server import create_app
from ados.core.config import ADOSConfig
from ados.core.health import HealthMonitor
from ados.core.service_tracker import ServiceTracker
from ados.services.mavlink.state import VehicleState


@pytest.fixture
def client(tmp_path, monkeypatch):
    monkeypatch.setenv("ADOS_PROFILE_OVERRIDE", "ground_station")

    app = MagicMock()
    cfg = ADOSConfig()
    cfg.agent.profile = "ground_station"
    app.config = cfg
    app.health = HealthMonitor()
    app.services = ServiceTracker()
    app._start_time = time.monotonic()
    app.uptime_seconds = 0.0
    app._vehicle_state = VehicleState()
    app._fc_connection = MagicMock()
    app._fc_connection.connected = False
    app._fc_connection.port = ""
    app._fc_connection.baud = 0
    app._tasks = []
    app._param_cache = None
    app.pairing_manager.is_paired = False
    return TestClient(create_app(app))


def test_pic_ws_backpressure_constant_is_present():
    """Sanity: the bounded queue constant exists in the route source."""
    import inspect

    import ados.api.routes.ground_station.ui as ui_mod

    src = inspect.getsource(ui_mod)
    assert "_PIC_WS_QUEUE_MAX = 100" in src, (
        "PIC WS queue must declare a maxsize so a slow client cannot "
        "grow the queue without limit"
    )
    # Sanity: drop-oldest pattern present
    assert "queue.get_nowait()" in src
    assert "pic_ws_backpressure_drop" in src


def test_pic_ws_queue_logger_is_imported():
    """The structlog logger used for backpressure warnings must be wired."""
    import ados.api.routes.ground_station.ui as ui_mod

    assert hasattr(ui_mod, "log"), "ui module must expose `log` for backpressure warnings"


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
