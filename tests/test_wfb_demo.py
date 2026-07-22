"""Tests for demo WFB-ng manager."""

from __future__ import annotations

import asyncio
import random

import pytest

from ados.services.wfb.demo import (
    _RX_PROOF_CYCLE,
    _RX_PROOF_GRACE,
    _RX_PROOF_HEARD_WINDOW,
    DemoWfbManager,
    _is_rf_unverified,
    _seconds_since_return_signal,
)
from ados.services.wfb.link_quality import LinkState, LinkStats


def _transmitting_demo() -> DemoWfbManager:
    """A demo manager past its startup sequence, i.e. actually injecting."""
    demo = DemoWfbManager()
    demo._tx_live = True
    return demo


def test_demo_initial_state():
    demo = DemoWfbManager()
    assert demo.state == LinkState.DISCONNECTED
    assert demo.interface == "wlan_demo"
    assert demo.channel == 149


def test_demo_get_status():
    demo = DemoWfbManager()
    status = demo.get_status()
    assert status["state"] == "disconnected"
    assert status["interface"] == "wlan_demo"
    assert status["channel"] == 149
    assert "rssi_dbm" in status
    assert "loss_percent" in status
    assert status["restart_count"] == 0


def test_demo_generate_stats():
    demo = DemoWfbManager()
    stats = demo._generate_stats(10.0)
    assert -100.0 < stats.rssi_dbm < -20.0
    assert stats.packets_received > 0
    assert stats.noise_dbm < 0
    assert stats.snr_db > 0
    assert stats.bitrate_kbps > 0
    assert stats.timestamp != ""


def test_demo_generate_stats_has_loss():
    demo = DemoWfbManager()
    # Run multiple times to check variance
    has_loss = False
    for i in range(20):
        stats = demo._generate_stats(float(i))
        if stats.packets_lost > 0:
            has_loss = True
            break
    assert has_loss, "Expected some packet loss in demo mode"


@pytest.mark.asyncio
async def test_demo_run_startup():
    demo = DemoWfbManager()
    task = asyncio.create_task(demo.run())

    # Wait for startup sequence to complete
    await asyncio.sleep(2.5)

    assert demo.state in (LinkState.CONNECTED, LinkState.DEGRADED)
    assert demo.monitor.sample_count > 0

    await demo.stop()
    task.cancel()
    try:
        await task
    except asyncio.CancelledError:
        pass


@pytest.mark.asyncio
async def test_demo_stop():
    demo = DemoWfbManager()
    task = asyncio.create_task(demo.run())
    await asyncio.sleep(2.0)

    await demo.stop()
    assert demo.state == LinkState.DISCONNECTED

    task.cancel()
    try:
        await task
    except asyncio.CancelledError:
        pass


@pytest.mark.asyncio
async def test_demo_generates_samples():
    demo = DemoWfbManager()
    task = asyncio.create_task(demo.run())

    # Let it run for 4 seconds (startup is ~1.5s, then ~1 sample/sec)
    await asyncio.sleep(4.0)

    count = demo.monitor.sample_count
    assert count >= 2, f"Expected at least 2 samples, got {count}"

    status = demo.get_status()
    assert status["rssi_dbm"] != -100.0  # Should have real values

    await demo.stop()
    task.cancel()
    try:
        await task
    except asyncio.CancelledError:
        pass


# --- simulated received-side proof -------------------------------------


@pytest.mark.parametrize(
    ("tx_live", "rx_proven", "expected"),
    [
        # Injecting into the void: the case the verdict exists to expose.
        (True, False, True),
        # Injecting and heard: the link is proven.
        (True, True, False),
        # Not injecting: idle, which the transmit watchdog owns, not this.
        (False, False, False),
        (False, True, False),
    ],
)
def test_is_rf_unverified_truth_table(tx_live, rx_proven, expected):
    assert _is_rf_unverified(tx_live, rx_proven) is expected


def test_return_signal_age_climbs_once_the_peer_goes_quiet():
    """The simulation models the age of the last return signal, not a flag."""
    # While the peer is answering a beacon has just landed.
    assert _seconds_since_return_signal(0.0) == 0.0
    assert _seconds_since_return_signal(_RX_PROOF_HEARD_WINDOW - 1.0) == 0.0
    # Once it goes quiet the proof starts ageing.
    assert _seconds_since_return_signal(_RX_PROOF_HEARD_WINDOW) == 0.0
    assert _seconds_since_return_signal(_RX_PROOF_HEARD_WINDOW + 4.0) == 4.0
    # It ages past the grace window before the cycle turns over.
    assert _seconds_since_return_signal(_RX_PROOF_CYCLE - 1.0) > _RX_PROOF_GRACE
    # The next cycle hears the peer again.
    assert _seconds_since_return_signal(_RX_PROOF_CYCLE) == 0.0


def test_degraded_outranks_an_unverified_transmit():
    """A genuinely bad MEASURED link is never masked as merely unproven."""
    demo = _transmitting_demo()
    bad = LinkStats(rssi_dbm=-80.0, loss_percent=0.5)
    assert demo._derive_state(bad, rx_proven=False) == LinkState.DEGRADED
    lossy = LinkStats(rssi_dbm=-50.0, loss_percent=40.0)
    assert demo._derive_state(lossy, rx_proven=False) == LinkState.DEGRADED


def test_a_clean_but_unproven_link_is_rf_unverified():
    """Injecting with no confirmed reception is not a connected link.

    An advancing transmit counter only proves frames were accepted, never that
    the energy reached a receiver, so a clean-looking sample with no proof
    behind it must not read as connected.
    """
    demo = _transmitting_demo()
    clean = LinkStats(rssi_dbm=-50.0, loss_percent=0.5)
    assert demo._derive_state(clean, rx_proven=False) == LinkState.RF_UNVERIFIED
    assert demo._derive_state(clean, rx_proven=True) == LinkState.CONNECTED


def test_an_idle_link_is_not_unverified():
    """Not transmitting is the idle case, not the transmitting-blind one."""
    demo = DemoWfbManager()  # never started: nothing is injecting
    clean = LinkStats(rssi_dbm=-50.0, loss_percent=0.5)
    assert demo._derive_state(clean, rx_proven=False) == LinkState.CONNECTED
    assert demo.rf_unverified is False
    assert demo.get_status()["rf_unverified"] is False


def test_demo_enters_and_leaves_rf_unverified_over_a_cycle():
    """The whole hear -> lose -> hear cycle is exercisable with no hardware."""
    random.seed(20260722)  # pin the RSSI/loss jitter so the sweep is stable
    demo = _transmitting_demo()
    seen = set()
    for second in range(int(_RX_PROOF_CYCLE) * 2):
        demo._tick(float(second))
        seen.add(demo.state)

    assert LinkState.RF_UNVERIFIED in seen, "the unverified state never fired"
    assert LinkState.CONNECTED in seen, "the link never recovered to connected"


def test_demo_status_agrees_with_the_unverified_state():
    """The state string and the booleans are views of the one simulated proof.

    A body reporting an unverified link as locked is the false-healthy surface
    this pairing exists to prevent, so the three are asserted on one body.
    """
    random.seed(20260722)
    demo = _transmitting_demo()
    unverified_bodies = 0
    for second in range(int(_RX_PROOF_CYCLE) * 2):
        demo._tick(float(second))
        status = demo.get_status()
        if status["state"] != LinkState.RF_UNVERIFIED.value:
            continue
        unverified_bodies += 1
        assert status["rf_unverified"] is True
        # Injecting blind is not a locked link.
        assert status["channel_locked"] is False

    assert unverified_bodies > 0, "no unverified status body was produced"


@pytest.mark.asyncio
async def test_stopping_clears_the_unverified_verdict():
    """Nothing is injecting once stopped, so the verdict clears with it."""
    demo = _transmitting_demo()
    demo._tick(_RX_PROOF_CYCLE - 1.0)  # deep in the silent stretch
    assert demo.rf_unverified is True

    await demo.stop()
    assert demo.rf_unverified is False
    assert demo.get_status()["rf_unverified"] is False
