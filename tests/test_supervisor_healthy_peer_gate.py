"""Tests for the periodic-scan skip gate on hop_supervisor.

Background: every periodic tick calls ``iw <iface> scan`` which locks
the radio for ~6 s. On a single-radio rig this drops wfb_tx frames
that share the same interface. When the control plane has decoded a
PresenceBeacon recently the link is fine and the rescan is wasted
work, so the supervisor should skip the scan in that case.

Three behaviors covered:
  1. Periodic tick with a fresh peer skips the scan (no _last_hop_at
     mutation, supervisor logs ``skip_periodic_link_healthy``).
  2. Reactive trigger always scans, even with a fresh peer.
  3. Cold-start periodic tick scans regardless of peer freshness so
     the drone can settle on a quiet channel before the ground rig
     comes up.
"""

from __future__ import annotations

import time
from unittest.mock import MagicMock, patch

import pytest

from ados.services.wfb.hop_supervisor import HopSupervisor


def _make_supervisor(
    peer_last_seen_unix: float | None,
    last_hop_at: float,
    *,
    reactive_loss: float = 0.0,
    reactive_rssi: float = -60.0,
) -> HopSupervisor:
    wfb = MagicMock()
    wfb._interface = "wlan0"
    wfb._channel = 36
    wfb._peer_last_seen_unix = peer_last_seen_unix

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
async def test_no_peer_ever_seen_scans() -> None:
    """Periodic tick scans when no peer has ever been observed.

    Cold-start signal is "PresenceBeacon never decoded" — gating on
    the older _last_hop_at would keep scanning forever on a rig
    where every hop aborts on no_peer_ack (the failure path the
    gate is supposed to protect).
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

    pick.assert_called_once()


@pytest.mark.asyncio
async def test_periodic_with_stale_peer_scans() -> None:
    """A periodic tick with a peer last seen >60 s ago resumes scanning."""
    sup = _make_supervisor(
        peer_last_seen_unix=time.time() - 120.0,
        last_hop_at=time.monotonic() - 300.0,
    )

    with patch(
        "ados.services.wfb.hop_supervisor._pick_target_channel",
        return_value=None,
    ) as pick:
        await sup._tick(periodic_due=True)

    pick.assert_called_once()


@pytest.mark.asyncio
async def test_periodic_with_no_peer_seen_yet_scans() -> None:
    """If no peer has ever been seen (None) the periodic scan must run."""
    sup = _make_supervisor(
        peer_last_seen_unix=None,
        last_hop_at=time.monotonic() - 300.0,
    )

    with patch(
        "ados.services.wfb.hop_supervisor._pick_target_channel",
        return_value=None,
    ) as pick:
        await sup._tick(periodic_due=True)

    pick.assert_called_once()
