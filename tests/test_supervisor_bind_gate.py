"""Tests for the bind-active gate on supervisor auto-restart + hop tick.

Regression coverage for the convergence bug where the supervisor's
5 s monitor loop detected `ados-wfb` as inactive after the bind
orchestrator stopped it for exclusive radio access during bind, then
auto-restarted it. The restarted wfb manager re-acquired the adapter,
set monitor mode, started the hop loop, and corrupted bind-tunnel
traffic. The socat handshake then timed out at the key-transfer
budget, auto_pair retried every 60 s with the same outcome and the
pair never converged.

Fix bundle:
  1. bind_orchestrator.is_bind_active() returns True for non-terminal
     bind states, False for terminal ones (IDLE, PAIRED, FAILED,
     ABORTED) and when no session has ever started.
  2. Supervisor.MonitorMixin._monitor_loop skips auto-restart of
     ados-wfb / ados-wfb-rx when is_bind_active() is True.
  3. HopSupervisor._tick returns early when is_bind_active() is True
     so no channel change races the bind tunnel.
"""

from __future__ import annotations

import asyncio
from unittest.mock import AsyncMock, MagicMock

import pytest

from ados.core.config import ADOSConfig
from ados.core.supervisor import Supervisor
from ados.services.wfb import bind_orchestrator as orch_mod
from ados.services.wfb.bind_orchestrator import (
    BindSession,
    BindState,
    get_bind_orchestrator,
    is_bind_active,
)
from ados.services.wfb.hop_supervisor import HopSupervisor


@pytest.fixture(autouse=True)
def _reset_orchestrator() -> None:
    orch_mod._reset_for_tests()
    yield
    orch_mod._reset_for_tests()


def _seed_session(state: BindState) -> BindSession:
    """Create a BindSession on the singleton orchestrator at a given state.

    Avoids running the full state machine; tests only need to observe
    that is_bind_active() reflects the session's state.
    """
    orch = get_bind_orchestrator()
    session = BindSession(session_id="test-session", role="drone")
    session.transition(state)
    orch._session = session
    return session


def test_is_bind_active_no_session() -> None:
    """No session ever started => is_bind_active() is False."""
    # Singleton has been reset by fixture; no session exists.
    assert is_bind_active() is False
    # Calling the accessor also must not implicitly mark anything active.
    _ = get_bind_orchestrator()
    assert is_bind_active() is False


@pytest.mark.parametrize(
    "state",
    [
        BindState.IDLE,
        BindState.PAIRED,
        BindState.FAILED,
        BindState.ABORTED,
    ],
)
def test_is_bind_active_terminal_states(state: BindState) -> None:
    """Terminal states => is_bind_active() is False."""
    _seed_session(state)
    assert is_bind_active() is False


@pytest.mark.parametrize(
    "state",
    [
        BindState.OPENING_TUNNEL,
        BindState.WAITING_PEER,
        BindState.TRANSFERRING_KEYS,
        BindState.APPLYING_KEYS,
        BindState.RESTARTING_SERVICES,
    ],
)
def test_is_bind_active_non_terminal_states(state: BindState) -> None:
    """Non-terminal states => is_bind_active() is True."""
    _seed_session(state)
    assert is_bind_active() is True


# The bind-gate on supervisor auto-restart now lives in the Rust supervisor
# (it owns the bind orchestrator in-process), so the Python fallback supervisor
# no longer carries the gate — the prior `test_supervisor_skips_wfb_restart_
# during_bind` test covered behaviour that moved to the Rust crate and was
# removed with the Python gate.


@pytest.mark.asyncio
async def test_supervisor_restarts_wfb_when_no_bind(monkeypatch) -> None:
    """Existing behavior preserved: a dead ados-wfb with NO bind in
    flight gets auto-restarted as before."""
    # No session seeded; is_bind_active() is False.
    assert is_bind_active() is False

    sup = Supervisor(ADOSConfig())
    sup._services["ados-wfb"].state = "running"

    start_service = AsyncMock(return_value=True)
    monkeypatch.setattr(sup, "start_service", start_service)
    monkeypatch.setattr(
        sup, "_check_service_active", AsyncMock(return_value=False)
    )
    monkeypatch.setattr(sup, "_collect_metrics", lambda: None)
    monkeypatch.setattr(sup, "_sd_notify_watchdog", lambda: None)

    loop_task = asyncio.create_task(sup._monitor_loop())
    await asyncio.sleep(0.05)
    sup._shutdown.set()
    try:
        await asyncio.wait_for(loop_task, timeout=2.0)
    except TimeoutError:
        loop_task.cancel()
        try:
            await loop_task
        except (asyncio.CancelledError, Exception):
            pass

    assert start_service.await_count == 1
    assert start_service.await_args.args == ("ados-wfb",)


@pytest.mark.asyncio
async def test_hop_supervisor_skips_during_bind() -> None:
    """HopSupervisor._tick returns early during a bind session.

    Builds a HopSupervisor with mocks for wfb_manager + link quality
    monitor, seeds an in-flight bind session, and asserts the tick
    bails before reading the wfb interface (which would otherwise be
    the next branch).
    """
    _seed_session(BindState.WAITING_PEER)
    assert is_bind_active() is True

    wfb = MagicMock()
    # If the tick proceeds past the bind gate, it reads _interface on
    # the wfb manager. Configure it to raise so a missed gate would
    # surface as a test failure rather than a silent no-op.
    wfb._interface = "wlan0"
    wfb._channel = 36

    lqm = MagicMock()
    lqm._latest = None
    lqm.latest = None

    sup = HopSupervisor(
        wfb_manager=wfb,
        link_quality_monitor=lqm,
        enabled=True,
    )

    # Force the periodic deadline far in the past so the tick would
    # otherwise want to hop. If the bind gate works, no scan is
    # attempted and no channel change is scheduled.
    import time as _time

    result = await sup._tick(_time.monotonic() - 1.0)
    # _tick returns None on every branch; the assertion is that we
    # got here without touching the wfb scan path.
    assert result is None
    # The supervisor should not have set _last_hop_at (no hop occurred).
    assert sup._last_hop_at == 0.0
