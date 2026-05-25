"""Tests for the wfb_tx zombie-detection branch of _tx_health_watchdog.

The watchdog declares a zombie when the radio's tx_bytes counter is
flat AND the process is still consuming UDP packets from its ingress
socket. The "consuming" signal moved from /proc/net/udp drops +
rx_queue (which both stay at 0 on a steady-state video pipe that
drains as fast as it arrives) to /proc/<wfb_tx_pid>/io rchar.

Three coverage cases:

  1. rchar advancing + tx_bytes flat → zombie, watchdog terminates
     the subprocess.
  2. rchar flat + tx_bytes flat → genuine idle, watchdog skips kill.
  3. rchar advancing + tx_bytes advancing → healthy, watchdog skips
     kill.
"""

from __future__ import annotations

import asyncio
from unittest.mock import MagicMock, patch

import pytest


_REAL_SLEEP = asyncio.sleep


async def _run_one_tick(watchdog_coro) -> None:
    """Drive the watchdog with the manager-module asyncio.sleep stubbed
    to yield instantly so the test runs in milliseconds. The real
    asyncio.sleep stays available under _REAL_SLEEP so other library
    code (asyncio.wait_for internals) keeps working.
    """
    async def _instant(_delay):
        await _REAL_SLEEP(0)

    with patch("ados.services.wfb.manager.asyncio.sleep", side_effect=_instant):
        try:
            await asyncio.wait_for(watchdog_coro, timeout=2.0)
        except asyncio.TimeoutError:
            pass


def _make_manager(
    *,
    tx_bytes_series: list[int],
    rchar_series: list[int | None],
    interface: str = "wlan0",
) -> MagicMock:
    """Construct a WfbManager-like mock with the watchdog method bound."""
    from ados.services.wfb.manager import WfbManager

    mgr = MagicMock(spec=WfbManager)
    mgr._interface = interface
    mgr._running = True

    tx_proc = MagicMock()
    tx_proc.pid = 9999
    tx_proc.returncode = None
    mgr._tx_proc = tx_proc

    # Counter-state attrs used by the watchdog
    mgr._last_tx_byte_value = -1
    mgr._last_tx_byte_change_at = 0.0
    mgr._last_upstream_rchar = -1
    mgr._last_upstream_byte_value = -1
    mgr._last_upstream_change_at = 0.0
    mgr._last_upstream_silent_log_at = 0.0
    mgr._tx_zombie_kills = 0

    # Bind the real watchdog + helper methods so the test exercises
    # the actual implementation.
    mgr._tx_health_watchdog = WfbManager._tx_health_watchdog.__get__(
        mgr, WfbManager
    )
    mgr._read_wfb_tx_consumed_bytes = (
        lambda series=iter(rchar_series): next(series, None)
    )
    mgr._read_wfb_tx_udp_state = lambda: None  # primary path only

    # Stop the watchdog loop after the series is exhausted.
    tx_state = {"i": 0}

    def _read_tx() -> int:
        idx = min(tx_state["i"], len(tx_bytes_series) - 1)
        tx_state["i"] += 1
        if tx_state["i"] >= len(tx_bytes_series):
            # Flip _running so the watchdog while-loop exits cleanly.
            mgr._running = False
        return tx_bytes_series[idx]

    return mgr, _read_tx


@pytest.mark.asyncio
async def test_zombie_rchar_advancing_tx_flat_triggers_terminate() -> None:
    """rchar advances while tx_bytes flat → watchdog terminates wfb_tx."""
    mgr, read_tx = _make_manager(
        tx_bytes_series=[1000, 1000, 1000, 1000, 1000, 1000, 1000, 1000],
        rchar_series=[500, 600, 700, 800, 900, 1000, 1100],
    )
    # Make the silence threshold tiny so the test doesn't wait.
    with patch(
        "ados.services.wfb.manager._TX_HEALTH_SILENCE_THRESHOLD_S", 0.0
    ), patch(
        "ados.services.wfb.manager._TX_HEALTH_POLL_INTERVAL_S", 0.0
    ), patch("builtins.open", create=True) as open_mock:
        open_mock.return_value.__enter__.return_value.read.side_effect = (
            lambda: str(read_tx())
        )
        await _run_one_tick(mgr._tx_health_watchdog())

    mgr._tx_proc.terminate.assert_called()
    assert mgr._tx_zombie_kills >= 1


@pytest.mark.asyncio
async def test_idle_rchar_flat_tx_flat_skips_kill() -> None:
    """rchar flat + tx_bytes flat → genuine idle, no kill."""
    mgr, read_tx = _make_manager(
        tx_bytes_series=[2000, 2000, 2000, 2000, 2000, 2000],
        rchar_series=[42, 42, 42, 42, 42, 42],
    )
    with patch(
        "ados.services.wfb.manager._TX_HEALTH_SILENCE_THRESHOLD_S", 0.0
    ), patch(
        "ados.services.wfb.manager._TX_HEALTH_POLL_INTERVAL_S", 0.0
    ), patch("builtins.open", create=True) as open_mock:
        open_mock.return_value.__enter__.return_value.read.side_effect = (
            lambda: str(read_tx())
        )
        await _run_one_tick(mgr._tx_health_watchdog())

    mgr._tx_proc.terminate.assert_not_called()
    assert mgr._tx_zombie_kills == 0


@pytest.mark.asyncio
async def test_healthy_both_advancing_skips_kill() -> None:
    """rchar advancing + tx_bytes advancing → healthy, no kill."""
    mgr, read_tx = _make_manager(
        tx_bytes_series=[100, 200, 300, 400, 500, 600],
        rchar_series=[50, 75, 100, 125, 150, 175],
    )
    with patch(
        "ados.services.wfb.manager._TX_HEALTH_SILENCE_THRESHOLD_S", 0.0
    ), patch(
        "ados.services.wfb.manager._TX_HEALTH_POLL_INTERVAL_S", 0.0
    ), patch("builtins.open", create=True) as open_mock:
        open_mock.return_value.__enter__.return_value.read.side_effect = (
            lambda: str(read_tx())
        )
        await _run_one_tick(mgr._tx_health_watchdog())

    mgr._tx_proc.terminate.assert_not_called()
    assert mgr._tx_zombie_kills == 0


def _make_recvq_manager(udp_series: list[tuple[int, int]]) -> MagicMock:
    """Construct a WfbManager-like mock with the per-stream video-tx
    backlog watchdog bound and its UDP-state reader driven by a series of
    (rx_queue, drops) tuples. Flips _running off once the series is
    exhausted so a no-kill case still terminates the loop.
    """
    from ados.services.wfb.manager import WfbManager

    mgr = MagicMock(spec=WfbManager)
    mgr._interface = "wlan0"
    mgr._running = True

    tx_proc = MagicMock()
    tx_proc.pid = 9999
    tx_proc.returncode = None
    mgr._tx_proc = tx_proc

    mgr._video_recvq_high_since = None
    mgr._last_video_recvq_bytes = 0
    mgr._tx_video_stalled = False
    mgr._tx_video_stall_kills = 0

    mgr._tx_video_recvq_watchdog = (
        WfbManager._tx_video_recvq_watchdog.__get__(mgr, WfbManager)
    )

    state = {"i": 0}

    def _read_udp() -> tuple[int, int] | None:
        idx = min(state["i"], len(udp_series) - 1)
        state["i"] += 1
        if state["i"] >= len(udp_series):
            mgr._running = False
        return udp_series[idx]

    mgr._read_wfb_tx_udp_state = _read_udp
    return mgr


@pytest.mark.asyncio
async def test_video_recvq_pinned_triggers_terminate() -> None:
    """UDP 5600 backlog pinned above the high-water mark while wfb_tx is
    alive → the video-tx watchdog terminates the subprocess even though
    the aggregate tx_bytes counter would still be moving from the control
    plane. This is the silent video-stall failure mode (rule 37)."""
    pinned = 4_195_072  # ~4 MiB, the rmem_max-pinned backlog seen on a
    # wedged wfb_tx -p 0
    mgr = _make_recvq_manager([(pinned, 0), (pinned, 0), (pinned, 0)])
    with patch(
        "ados.services.wfb.manager._TX_VIDEO_RECVQ_BACKLOG_THRESHOLD_S", 0.0
    ), patch(
        "ados.services.wfb.manager._TX_HEALTH_POLL_INTERVAL_S", 0.0
    ):
        await _run_one_tick(mgr._tx_video_recvq_watchdog())

    mgr._tx_proc.terminate.assert_called()
    assert mgr._tx_video_stall_kills == 1
    assert mgr._tx_video_stalled is True


@pytest.mark.asyncio
async def test_video_recvq_drained_skips_kill() -> None:
    """Backlog stays near zero (healthy drain, or genuinely idle) → no
    kill, no stall flag."""
    mgr = _make_recvq_manager([(0, 0), (128, 0), (0, 0), (64, 0)])
    with patch(
        "ados.services.wfb.manager._TX_VIDEO_RECVQ_BACKLOG_THRESHOLD_S", 0.0
    ), patch(
        "ados.services.wfb.manager._TX_HEALTH_POLL_INTERVAL_S", 0.0
    ):
        await _run_one_tick(mgr._tx_video_recvq_watchdog())

    mgr._tx_proc.terminate.assert_not_called()
    assert mgr._tx_video_stall_kills == 0
    assert mgr._tx_video_stalled is False
