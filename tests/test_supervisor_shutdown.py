"""Tests for Supervisor shutdown ordering.

The HTTP frontend (ados-api) must stop before the hardware services it
queries, so in-flight /api/video / /api/wfb requests do not return 500
while the underlying service is dying. Between tiers we wait for
systemctl is-active to clear before tearing down the next tier.
"""

from __future__ import annotations

from unittest.mock import AsyncMock, MagicMock

import pytest

from ados.core.config import ADOSConfig
from ados.core.supervisor import Supervisor


def _supervisor_with_running(*names: str) -> Supervisor:
    s = Supervisor(ADOSConfig())
    for n in names:
        spec = s._services.get(n)
        if spec is not None:
            spec.state = "running"
    return s


@pytest.mark.asyncio
async def test_api_stops_before_hardware(monkeypatch):
    """ados-api must be in the first stop_service call."""
    sup = _supervisor_with_running("ados-api", "ados-mavlink", "ados-video", "ados-wfb")
    stop_order: list[str] = []

    async def fake_stop(name: str) -> bool:
        stop_order.append(name)
        spec = sup._services.get(name)
        if spec:
            spec.state = "stopped"
        return True

    monkeypatch.setattr(sup, "stop_service", fake_stop)
    monkeypatch.setattr(sup, "_wait_for_stop", AsyncMock())

    await sup.stop()

    assert stop_order, "no services stopped"
    assert stop_order[0] == "ados-api", (
        f"ados-api must stop first; actual order: {stop_order}"
    )


@pytest.mark.asyncio
async def test_hardware_stops_before_remaining_core(monkeypatch):
    """After ados-api, hardware tier must drain before mavlink/cloud/health."""
    sup = _supervisor_with_running(
        "ados-api", "ados-mavlink", "ados-cloud", "ados-video"
    )
    stop_order: list[str] = []

    async def fake_stop(name: str) -> bool:
        stop_order.append(name)
        spec = sup._services.get(name)
        if spec:
            spec.state = "stopped"
        return True

    monkeypatch.setattr(sup, "stop_service", fake_stop)
    monkeypatch.setattr(sup, "_wait_for_stop", AsyncMock())

    await sup.stop()

    api_idx = stop_order.index("ados-api")
    video_idx = stop_order.index("ados-video")
    mavlink_idx = stop_order.index("ados-mavlink")

    assert api_idx == 0
    assert video_idx < mavlink_idx, (
        f"hardware (video) must stop before remaining core (mavlink); "
        f"actual order: {stop_order}"
    )


@pytest.mark.asyncio
async def test_wait_for_stop_called_per_tier(monkeypatch):
    """_wait_for_stop must be called between every tier so we don't tear
    down the next tier while the previous is still running."""
    sup = _supervisor_with_running(
        "ados-api", "ados-mavlink", "ados-video", "ados-scripting"
    )
    wait_calls: list[list[str]] = []

    async def fake_wait(names, timeout_secs=5.0):
        wait_calls.append(list(names))

    async def fake_stop(name: str) -> bool:
        spec = sup._services.get(name)
        if spec:
            spec.state = "stopped"
        return True

    monkeypatch.setattr(sup, "stop_service", fake_stop)
    monkeypatch.setattr(sup, "_wait_for_stop", fake_wait)

    await sup.stop()

    # Tier 0 (frontend), tier 1 (suite), tier 2 (hardware), tier 3 (ondemand), tier 4 (core)
    # = 5 wait calls regardless of whether each tier had services to stop.
    assert len(wait_calls) == 5, f"expected 5 tier waits, got {len(wait_calls)}"
    # First wait must include ados-api
    assert wait_calls[0] == ["ados-api"]


@pytest.mark.asyncio
async def test_stop_does_not_double_stop_api(monkeypatch):
    """ados-api stops once in tier 0; the per-tier loop must skip it."""
    sup = _supervisor_with_running("ados-api", "ados-mavlink")
    stop_count: dict[str, int] = {}

    async def fake_stop(name: str) -> bool:
        stop_count[name] = stop_count.get(name, 0) + 1
        spec = sup._services.get(name)
        if spec:
            spec.state = "stopped"
        return True

    monkeypatch.setattr(sup, "stop_service", fake_stop)
    monkeypatch.setattr(sup, "_wait_for_stop", AsyncMock())

    await sup.stop()

    assert stop_count.get("ados-api", 0) == 1, (
        f"ados-api stopped {stop_count.get('ados-api', 0)} times, expected 1"
    )


@pytest.mark.asyncio
async def test_stop_skips_services_not_running(monkeypatch):
    """A service in `stopped` state must not be re-stopped during shutdown."""
    sup = _supervisor_with_running("ados-mavlink")
    # ados-api is registered but not running
    stopped: list[str] = []

    async def fake_stop(name: str) -> bool:
        stopped.append(name)
        return True

    monkeypatch.setattr(sup, "stop_service", fake_stop)
    monkeypatch.setattr(sup, "_wait_for_stop", AsyncMock())

    await sup.stop()

    assert "ados-api" not in stopped
    assert "ados-mavlink" in stopped


@pytest.mark.asyncio
async def test_wait_for_stop_returns_quickly_when_all_inactive(monkeypatch):
    """When is-active reports inactive, _wait_for_stop returns without
    waiting for the full timeout."""
    sup = Supervisor(ADOSConfig())

    monkeypatch.setattr(Supervisor, "_is_active", staticmethod(lambda name: False))

    import time as _time
    start = _time.monotonic()
    await sup._wait_for_stop(["ados-mavlink", "ados-video"], timeout_secs=5.0)
    elapsed = _time.monotonic() - start
    assert elapsed < 0.5, f"_wait_for_stop should return early; took {elapsed}s"


@pytest.mark.asyncio
async def test_wait_for_stop_empty_list_is_noop():
    """Passing an empty list should return immediately without polling."""
    sup = Supervisor(ADOSConfig())
    import time as _time
    start = _time.monotonic()
    await sup._wait_for_stop([], timeout_secs=5.0)
    elapsed = _time.monotonic() - start
    assert elapsed < 0.05
