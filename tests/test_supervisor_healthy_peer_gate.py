"""Tests for the rendezvous-first scan gate on hop_supervisor.

Background: both rigs come up on a fixed home channel and bring the
link up there. Only AFTER the link is established (a peer was seen at
least once) does coordinated hopping turn on. Before that the drone
stays on the home channel transmitting and runs NO ``iw scan``. A
scan would strand the iface in managed mode and pull the radio off the
home channel, which is the divergence that kept the two sides from
meeting.

Once linked, every periodic tick that does scan calls ``iw <iface>
scan`` which locks the radio for ~6 s. On a single-radio rig this
drops wfb_tx frames that share the same interface. When the control
plane decoded a PresenceBeacon recently the link is fine and the
rescan is wasted work, so the supervisor skips the scan in that case.

Behaviors covered:
  1. Periodic tick with a fresh peer skips the scan (no _last_hop_at
     mutation, supervisor logs ``skip_periodic_link_healthy``).
  2. Reactive trigger scans when the peer is fresh and linked.
  3. Cold start (no peer ever seen) does NOT scan. The drone holds
     the home channel until the link is established.
  4. A peer that went stale after being linked falls back to the home
     channel instead of scanning.
"""

from __future__ import annotations

import time
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from ados.services.wfb.hop_supervisor import HopSupervisor


def _make_supervisor(
    peer_last_seen_unix: float | None,
    last_hop_at: float,
    *,
    reactive_loss: float = 0.0,
    reactive_rssi: float = -60.0,
    channel: int = 153,
    home_channel: int = 149,
) -> HopSupervisor:
    wfb = MagicMock()
    wfb._interface = "wlan0"
    wfb._channel = channel
    wfb._peer_last_seen_unix = peer_last_seen_unix
    # Async surface used by the home-channel fallback path.
    wfb.stop = AsyncMock()
    wfb.start_tx = AsyncMock(return_value=True)
    wfb.start_tx_control = AsyncMock(return_value=True)
    wfb.start_rx_control = AsyncMock(return_value=True)

    lqm = MagicMock()
    latest = MagicMock()
    # Stamp packets_received + timestamp so the reactive trigger has
    # "real data" to evaluate. The reactive threshold uses both
    # loss_percent and rssi_dbm; defaults keep us below trigger unless
    # explicitly overridden via the fixture args.
    latest.timestamp = "2026-05-23T11:30:00+00:00"
    latest.packets_received = 1000
    latest.loss_percent = reactive_loss
    latest.rssi_dbm = reactive_rssi
    lqm._latest = latest
    lqm.latest = latest

    sup = HopSupervisor(
        wfb_manager=wfb,
        link_quality_monitor=lqm,
        enabled=True,
        home_channel=home_channel,
    )
    sup._last_hop_at = last_hop_at
    return sup


@pytest.mark.asyncio
async def test_periodic_with_fresh_peer_skips_scan() -> None:
    """A periodic tick must skip the scan when the peer was seen <60 s ago."""
    sup = _make_supervisor(
        peer_last_seen_unix=time.time() - 10.0,
        last_hop_at=time.monotonic() - 300.0,
    )

    with patch(
        "ados.services.wfb.hop_supervisor._pick_target_channel"
    ) as pick:
        await sup._tick(periodic_due=True)

    pick.assert_not_called()
    assert sup._last_hop_at != 0.0  # untouched (no hop executed)


@pytest.mark.asyncio
async def test_reactive_trigger_scans_despite_fresh_peer() -> None:
    """A reactive trigger (loss + cooldown elapsed) bypasses the freshness gate."""
    # Loss above threshold (default 5%) AND reactive cooldown (default
    # 30 s) elapsed since the last hop fires the reactive trigger.
    sup = _make_supervisor(
        peer_last_seen_unix=time.time() - 5.0,
        last_hop_at=time.monotonic() - 120.0,
        reactive_loss=20.0,
    )

    with patch(
        "ados.services.wfb.hop_supervisor._pick_target_channel",
        return_value=None,
    ) as pick:
        await sup._tick(periodic_due=False)

    pick.assert_called_once()


@pytest.mark.asyncio
async def test_no_peer_ever_seen_does_not_scan() -> None:
    """Cold start (no peer ever seen) must NOT scan.

    Rendezvous-first: the drone holds the home channel transmitting
    until the link is established. Running an iw scan here would strand
    the iface in managed mode and pull the radio off the home channel.
    """
    sup = _make_supervisor(
        peer_last_seen_unix=None,
        last_hop_at=0.0,
    )

    with patch(
        "ados.services.wfb.hop_supervisor._pick_target_channel",
        return_value=None,
    ) as pick:
        await sup._tick(periodic_due=True)

    pick.assert_not_called()


@pytest.mark.asyncio
async def test_periodic_with_stale_peer_falls_back_home() -> None:
    """A peer last seen past the stale window falls back to home, not scan.

    Once the link was up and the peer goes quiet, the drone returns to
    the home / rendezvous channel so the ground rig (which also falls
    back to home) can re-find it. It does NOT scan for a quiet channel
    the peer is no longer on.
    """
    sup = _make_supervisor(
        peer_last_seen_unix=time.time() - 120.0,
        last_hop_at=time.monotonic() - 300.0,
        channel=153,
        home_channel=149,
    )

    with patch(
        "ados.services.wfb.hop_supervisor._pick_target_channel",
        return_value=None,
    ) as pick, patch(
        "asyncio.create_subprocess_exec",
        new=AsyncMock(),
    ):
        await sup._tick(periodic_due=True)

    pick.assert_not_called()
    # Returned to the home channel via the manager's restart surface.
    sup._wfb.stop.assert_awaited_once()
    sup._wfb.start_tx.assert_awaited_once()
    assert sup._wfb._channel == 149


@pytest.mark.asyncio
async def test_no_peer_seen_yet_does_not_scan() -> None:
    """If no peer has ever been seen (None) the periodic scan must NOT run."""
    sup = _make_supervisor(
        peer_last_seen_unix=None,
        last_hop_at=time.monotonic() - 300.0,
    )

    with patch(
        "ados.services.wfb.hop_supervisor._pick_target_channel",
        return_value=None,
    ) as pick:
        await sup._tick(periodic_due=True)

    pick.assert_not_called()
