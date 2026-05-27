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
    # The configured rendezvous home channel. The cold-start self-heal
    # path reads it to return there after a sweep finds nothing.
    mgr._config = MagicMock()
    mgr._config.channel = 149
    # Cold-start home-hold bookkeeping. Default to "just started" and the
    # one-shot sweep not yet done, so the cold-start hold branch is
    # exercised within the hold budget (no sweep) unless a test overrides.
    mgr._cold_start_at = time.monotonic()
    mgr._cold_sweep_done = False
    # The watchdog sweep only runs after a link has been established and
    # then lost. These cases model exactly that, so the rig is marked as
    # having been linked. A cold start (never linked) holds the home
    # channel instead and is covered by its own tests below.
    mgr._ever_linked = True
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


@pytest.mark.asyncio
async def test_cold_start_never_linked_holds_home_no_sweep():
    """Cold start (never linked) + silent + no peer → hold home, no sweep.

    Rendezvous-first: until a lock has been established, the transmitter
    is broadcasting on the fixed home channel, so the receiver waits
    there. A blind cold sweep would pull it off the home channel the
    drone is transmitting on.
    """
    mgr = _make_manager(video_silent=True, peer_present=False)
    mgr._ever_linked = False
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

    # Never linked: no sweep, no kill, stay on the home channel.
    mgr._acquirer.mark_unlocked.assert_not_called()
    mgr._acquirer.acquire.assert_not_awaited()
    mgr._acquirer.acquire_target.assert_not_awaited()
    mgr._rx_proc.terminate.assert_not_called()
    assert mgr._channel == 149
    assert mgr._reacquire_kills == 0


@pytest.mark.asyncio
async def test_cold_start_budget_elapsed_runs_one_self_heal_sweep():
    """Cold start unlinked past the hold budget → one acquire sweep.

    Holding the home channel forever would deadlock a pair whose home
    channels are mismatched. After the bounded hold the receiver runs one
    self-heal sweep; a successful lock relocates and persists the channel.
    """
    mgr = _make_manager(video_silent=True, peer_present=False)
    mgr._ever_linked = False
    mgr._cold_start_at = time.monotonic()
    mgr._cold_sweep_done = False
    mgr._acquirer = MagicMock()
    mgr._acquirer.mark_unlocked = MagicMock()
    mgr._acquirer.acquire = AsyncMock(return_value=157)
    mgr._acquirer.acquire_target = AsyncMock(return_value=True)
    mgr._acquirer.try_channel = AsyncMock(return_value=True)

    with patch(
        "ados.services.ground_station.wfb_rx._VALID_RX_SILENCE_THRESHOLD_S",
        0.0,
    ), patch(
        "ados.services.ground_station.wfb_rx._VALID_RX_POLL_INTERVAL_S",
        0.0,
    ), patch(
        # Zero hold budget so the freshly-seeded cold-start timer is
        # already past it on the first poll.
        "ados.services.ground_station.wfb_rx._COLD_START_HOME_HOLD_S",
        0.0,
    ):
        await _run_watchdog(mgr._valid_packet_watchdog())

    mgr._acquirer.acquire.assert_awaited()
    assert mgr._channel == 157
    mgr._persist_locked_channel.assert_called_with(157)
    assert mgr._cold_sweep_done is True
    mgr._rx_proc.terminate.assert_not_called()


@pytest.mark.asyncio
async def test_cold_start_budget_elapsed_sweep_fails_returns_home():
    """Cold self-heal sweep finds nothing → return to the home channel.

    A failed cold sweep must NOT terminate wfb_rx (the process is fine,
    the peer just isn't there yet); it returns to the home channel where
    the drone homes and resumes holding (one-shot, no thrash).
    """
    mgr = _make_manager(video_silent=True, peer_present=False)
    mgr._ever_linked = False
    mgr._channel = 161  # drifted off home from an earlier attempt
    mgr._config.channel = 149
    mgr._cold_start_at = time.monotonic()
    mgr._cold_sweep_done = False
    mgr._acquirer = MagicMock()
    mgr._acquirer.mark_unlocked = MagicMock()
    mgr._acquirer.acquire = AsyncMock(return_value=None)
    mgr._acquirer.acquire_target = AsyncMock(return_value=False)
    mgr._acquirer.try_channel = AsyncMock(return_value=True)

    with patch(
        "ados.services.ground_station.wfb_rx._VALID_RX_SILENCE_THRESHOLD_S",
        0.0,
    ), patch(
        "ados.services.ground_station.wfb_rx._VALID_RX_POLL_INTERVAL_S",
        0.0,
    ), patch(
        "ados.services.ground_station.wfb_rx._COLD_START_HOME_HOLD_S",
        0.0,
    ):
        await _run_watchdog(mgr._valid_packet_watchdog())

    mgr._acquirer.acquire.assert_awaited()
    # Returned to the home channel; no destructive restart.
    mgr._acquirer.try_channel.assert_awaited_with(149)
    assert mgr._channel == 149
    mgr._rx_proc.terminate.assert_not_called()
    assert mgr._reacquire_kills == 0
    assert mgr._cold_sweep_done is True


@pytest.mark.asyncio
async def test_silent_marginal_presence_gap_holds_no_sweep():
    """Linked + silent + presence gap inside the loss window → hold home.

    A marginal control-plane link drops presence beacons for tens of
    seconds at a time. As long as the peer was seen within the loss
    window the link is still paired-idle: hold the home channel rather
    than sweep (which leaves the channel the drone transmits on) or kill
    wfb_rx (which drops the control plane) — the self-inflicted thrash
    this guards against.
    """
    mgr = _make_manager(video_silent=True, peer_present=False)
    # Peer last seen 60 s ago: past the 30 s fresh window but well inside
    # the 120 s loss window.
    mgr._peer_presence_age_s = MagicMock(return_value=60.0)
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

    mgr._acquirer.mark_unlocked.assert_not_called()
    mgr._acquirer.acquire.assert_not_awaited()
    mgr._acquirer.acquire_target.assert_not_awaited()
    mgr._rx_proc.terminate.assert_not_called()
    assert mgr._channel == 149
    assert mgr._reacquire_kills == 0


@pytest.mark.asyncio
async def test_silent_presence_lost_beyond_window_sweeps():
    """Linked + silent + presence gone past the loss window → genuine loss.

    Once the peer has been absent longer than the loss window the link is
    treated as truly down and the reacquisition sweep runs.
    """
    mgr = _make_manager(video_silent=True, peer_present=False)
    # Peer last seen 200 s ago: beyond the 120 s loss window.
    mgr._peer_presence_age_s = MagicMock(return_value=200.0)
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
