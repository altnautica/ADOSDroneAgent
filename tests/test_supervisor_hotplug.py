"""Tests for hot-plug-driven service restart coalescing.

Per-device debounce already covered the SpeedyBee DFU<->flight transition,
but if multiple devices in the same service category hot-plug within the
500ms kernel-settle window, the supervisor used to spawn one fire-and-
forget restart task per event. Now they coalesce: latest event wins,
prior in-flight task for the same service is cancelled.
"""

from __future__ import annotations

import asyncio
from unittest.mock import AsyncMock

import pytest

from ados.core.config import ADOSConfig
from ados.core.supervisor import Supervisor


@pytest.mark.asyncio
async def test_back_to_back_events_for_same_service_coalesce(monkeypatch):
    """Two hot-plug restarts scheduled in quick succession should result
    in only ONE actual restart_service call."""
    sup = Supervisor(ADOSConfig())
    restart = AsyncMock(return_value=True)
    monkeypatch.setattr(sup, "restart_service", restart)

    sup._schedule_hotplug_restart("ados-video")
    # Second event lands within the 500ms window
    await asyncio.sleep(0.05)
    sup._schedule_hotplug_restart("ados-video")

    # Wait for the kernel-settle window + restart to complete
    await asyncio.sleep(0.7)
    # Drain any cancellations
    pending = [
        t for t in sup._hotplug_restart_tasks.values() if not t.done()
    ]
    if pending:
        await asyncio.gather(*pending, return_exceptions=True)

    # Only the latest scheduled task should have run restart_service
    assert restart.await_count == 1, (
        f"expected 1 restart, got {restart.await_count}"
    )


@pytest.mark.asyncio
async def test_different_services_run_concurrently(monkeypatch):
    """Hot-plug events for different services should both fire."""
    sup = Supervisor(ADOSConfig())
    restart = AsyncMock(return_value=True)
    monkeypatch.setattr(sup, "restart_service", restart)

    sup._schedule_hotplug_restart("ados-video")
    sup._schedule_hotplug_restart("ados-mavlink")

    await asyncio.sleep(0.7)
    pending = [
        t for t in sup._hotplug_restart_tasks.values() if not t.done()
    ]
    if pending:
        await asyncio.gather(*pending, return_exceptions=True)

    called_with = sorted(c.args[0] for c in restart.await_args_list)
    assert called_with == ["ados-mavlink", "ados-video"]


@pytest.mark.asyncio
async def test_completed_task_clears_tracker(monkeypatch):
    sup = Supervisor(ADOSConfig())
    monkeypatch.setattr(sup, "restart_service", AsyncMock(return_value=True))

    sup._schedule_hotplug_restart("ados-video")
    # Allow the task to complete naturally
    await asyncio.sleep(0.7)
    assert "ados-video" not in sup._hotplug_restart_tasks


@pytest.mark.asyncio
async def test_coalesce_cancels_pending_sleep(monkeypatch):
    """Cancelling a task during the 500ms sleep must not run restart_service
    for the cancelled scheduling — it should be cleanly aborted."""
    sup = Supervisor(ADOSConfig())
    restart = AsyncMock(return_value=True)
    monkeypatch.setattr(sup, "restart_service", restart)

    # Schedule, then immediately cancel by scheduling again
    sup._schedule_hotplug_restart("ados-video")
    sup._schedule_hotplug_restart("ados-video")
    sup._schedule_hotplug_restart("ados-video")
    sup._schedule_hotplug_restart("ados-video")

    await asyncio.sleep(0.7)
    pending = [
        t for t in sup._hotplug_restart_tasks.values() if not t.done()
    ]
    if pending:
        await asyncio.gather(*pending, return_exceptions=True)

    # Even though we scheduled 4 times, only the final one runs
    assert restart.await_count == 1


@pytest.mark.asyncio
async def test_supervisor_stop_cancels_in_flight_restarts(monkeypatch):
    """Hot-plug restart tasks in their kernel-settle sleep must be
    cancelled when the supervisor shuts down, not allowed to race the
    shutdown stop_service calls."""
    sup = Supervisor(ADOSConfig())
    restart = AsyncMock(return_value=True)
    monkeypatch.setattr(sup, "restart_service", restart)
    monkeypatch.setattr(sup, "stop_service", AsyncMock(return_value=True))
    monkeypatch.setattr(sup, "_wait_for_stop", AsyncMock())

    sup._schedule_hotplug_restart("ados-video")
    # Trigger shutdown almost immediately (well inside the 500ms window)
    await asyncio.sleep(0.05)
    await sup.stop()

    assert sup._hotplug_restart_tasks == {}
    # restart_service must NOT have been called — task was cancelled
    # during its sleep
    assert restart.await_count == 0
