"""Tests for demo WFB-ng manager."""

from __future__ import annotations

import asyncio

import pytest

from ados.services.wfb.demo import DemoWfbManager
from ados.services.wfb.manager import LinkState


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
