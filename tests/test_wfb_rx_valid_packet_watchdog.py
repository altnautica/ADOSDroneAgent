"""Tests for the receive-side valid-packet watchdog.

The watchdog watches `_last_valid_rx_change_at` (refreshed on every
interval that decoded a valid video packet). A stale timestamp means no
video is arriving. But silent video alone is NOT a fault: a paired link
with the drone simply not transmitting video decodes zero video packets,
which is normal. The sweep/kill is therefore gated on PEER PRESENCE —
only when video is silent AND no recent presence beacon was decoded does
the watchdog reacquire (and only kill if reacquisition fails).

Coverage cases:

  1. Video flowing (timestamp fresh) → watchdog does nothing.
  2. Video silent BUT peer present → log "paired, no video", no sweep.
  3. Video silent AND no peer, reacquire succeeds → channel relocked.
  4. Video silent AND no peer, reacquire fails → terminate for restart.
  5. Video silent, no peer, peer announced a channel → beacon-guided
     lock tried before the blind sweep.
"""

from __future__ import annotations

import asyncio
import time
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

_REAL_SLEEP = asyncio.sleep


async def _run_watchdog(coro) -> None:
    async def _instant(_delay):
        await _REAL_SLEEP(0)

    with patch(
        "ados.services.ground_station.wfb_rx.asyncio.sleep",
        side_effect=_instant,
    ):
        try:
            await asyncio.wait_for(coro, timeout=2.0)
        except asyncio.TimeoutError:
            pass


def _make_manager(
    *,
    video_silent: bool,
    peer_present: bool,
    announced_channel: int | None = None,
):
    """Build a WfbRxManager-like mock with the watchdog bound.

    ``video_silent`` controls whether ``_last_valid_rx_change_at`` is
    stale (no recent decode) or fresh. ``peer_present`` drives the
    peer-presence gate. The loop runs exactly one poll iteration: after
    the first tick the manager flips ``_running`` off so the while-loop
    exits even on the no-action path.
    """
    from ados.services.ground_station.wfb_rx import WfbRxManager

    mgr = MagicMock(spec=WfbRxManager)
    mgr._interface = "wlan0"
    mgr._running = True
    mgr._channel = 149
    mgr._reacquire_kills = 0
    # Stale timestamp (silent) or fresh timestamp (video flowing).
    mgr._last_valid_rx_change_at = (
        0.0 if video_silent else time.monotonic()
    )

    rx_proc = MagicMock()
    rx_proc.pid = 7777
    rx_proc.returncode = None
    mgr._rx_proc = rx_proc

    mgr._persist_locked_channel = MagicMock()

    # Peer-presence gate. Each call to _peer_present also advances a
    # one-shot latch that ends the loop after the first iteration.
    poll_state = {"i": 0}

    def _peer_present() -> bool:
        poll_state["i"] += 1
        if poll_state["i"] >= 1:
            mgr._running = False
        return peer_present

    mgr._peer_present = _peer_present
    mgr._peer_presence_age_s = MagicMock(
        return_value=5.0 if peer_present else None
    )
    mgr._peer_announced_channel = MagicMock(return_value=announced_channel)

    mgr._valid_packet_watchdog = (
        WfbRxManager._valid_packet_watchdog.__get__(mgr, WfbRxManager)
    )
    return mgr


@pytest.mark.asyncio
async def test_video_flowing_no_action():
    """Fresh decode timestamp → silence window not tripped, no sweep."""
    mgr = _make_manager(video_silent=False, peer_present=False)
    # Loop never trips the silence branch; flip _running off after one
    # poll so the loop terminates cleanly.
    state = {"i": 0}

    def _present():
        state["i"] += 1
        return False

    mgr._peer_present = _present
    mgr._acquirer = MagicMock()
    mgr._acquirer.acquire = AsyncMock(return_value=149)

    # Run a couple of ticks then stop.
    async def _stopper():
        await _REAL_SLEEP(0)
        mgr._running = False

    with patch(
        "ados.services.ground_station.wfb_rx._VALID_RX_SILENCE_THRESHOLD_S",
        9999.0,  # never trips with a fresh timestamp
    ), patch(
        "ados.services.ground_station.wfb_rx._VALID_RX_POLL_INTERVAL_S",
        0.0,
    ):
        await asyncio.gather(
            _run_watchdog(mgr._valid_packet_watchdog()), _stopper()
        )

    mgr._acquirer.acquire.assert_not_awaited()
    mgr._rx_proc.terminate.assert_not_called()
    assert mgr._reacquire_kills == 0


@pytest.mark.asyncio
async def test_silent_but_peer_present_does_not_sweep_or_kill():
    """THE KEY CASE: paired link, drone not sending video → no sweep."""
    mgr = _make_manager(video_silent=True, peer_present=True)
    mgr._acquirer = MagicMock()
    mgr._acquirer.mark_unlocked = MagicMock()
    mgr._acquirer.acquire = AsyncMock(return_value=157)
    mgr._acquirer.acquire_target = AsyncMock(return_value=True)

    with patch(
        "ados.services.ground_station.wfb_rx._VALID_RX_SILENCE_THRESHOLD_S",
        0.0,
    ), patch(
        "ados.services.ground_station.wfb_rx._VALID_RX_POLL_INTERVAL_S",
        0.0,
    ):
        await _run_watchdog(mgr._valid_packet_watchdog())

    # Peer present means the link is healthy-idle: NO sweep, NO kill.
    mgr._acquirer.mark_unlocked.assert_not_called()
    mgr._acquirer.acquire.assert_not_awaited()
    mgr._acquirer.acquire_target.assert_not_awaited()
    mgr._rx_proc.terminate.assert_not_called()
    assert mgr._reacquire_kills == 0


@pytest.mark.asyncio
async def test_silent_no_peer_reacquire_succeeds_no_terminate():
    """Silent + no peer → sweep; a successful lock avoids a kill."""
    mgr = _make_manager(video_silent=True, peer_present=False)
    mgr._acquirer = MagicMock()
    mgr._acquirer.mark_unlocked = MagicMock()
    mgr._acquirer.acquire = AsyncMock(return_value=157)

    with patch(
        "ados.services.ground_station.wfb_rx._VALID_RX_SILENCE_THRESHOLD_S",
        0.0,
    ), patch(
        "ados.services.ground_station.wfb_rx._VALID_RX_POLL_INTERVAL_S",
        0.0,
    ):
        await _run_watchdog(mgr._valid_packet_watchdog())

    mgr._acquirer.acquire.assert_awaited()
    assert mgr._channel == 157
    mgr._persist_locked_channel.assert_called_with(157)
    mgr._rx_proc.terminate.assert_not_called()
    assert mgr._reacquire_kills == 0


@pytest.mark.asyncio
async def test_silent_no_peer_reacquire_fails_terminates():
    """Silent + no peer + failed sweep → terminate wfb_rx for restart."""
    mgr = _make_manager(video_silent=True, peer_present=False)
    mgr._acquirer = MagicMock()
    mgr._acquirer.mark_unlocked = MagicMock()
    mgr._acquirer.acquire = AsyncMock(return_value=None)

    with patch(
        "ados.services.ground_station.wfb_rx._VALID_RX_SILENCE_THRESHOLD_S",
        0.0,
    ), patch(
        "ados.services.ground_station.wfb_rx._VALID_RX_POLL_INTERVAL_S",
        0.0,
    ):
        await _run_watchdog(mgr._valid_packet_watchdog())

    mgr._acquirer.acquire.assert_awaited()
    mgr._rx_proc.terminate.assert_called()
    assert mgr._reacquire_kills == 1


@pytest.mark.asyncio
async def test_silent_no_peer_beacon_guided_lock_tried_first():
    """An announced channel is tried with one dwell before a blind sweep."""
    mgr = _make_manager(
        video_silent=True, peer_present=False, announced_channel=44
    )
    mgr._acquirer = MagicMock()
    mgr._acquirer.mark_unlocked = MagicMock()
    mgr._acquirer.acquire_target = AsyncMock(return_value=True)
    mgr._acquirer.acquire = AsyncMock(return_value=161)

    with patch(
        "ados.services.ground_station.wfb_rx._VALID_RX_SILENCE_THRESHOLD_S",
        0.0,
    ), patch(
        "ados.services.ground_station.wfb_rx._VALID_RX_POLL_INTERVAL_S",
        0.0,
    ):
        await _run_watchdog(mgr._valid_packet_watchdog())

    # Beacon-guided lock to the announced channel succeeded — the blind
    # sweep must not have been needed.
    mgr._acquirer.acquire_target.assert_awaited_with(44)
    mgr._acquirer.acquire.assert_not_awaited()
    assert mgr._channel == 44
    mgr._persist_locked_channel.assert_called_with(44)
    mgr._rx_proc.terminate.assert_not_called()
