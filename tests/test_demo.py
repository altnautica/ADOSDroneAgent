"""Tests for demo mode — DemoFCConnection generates valid simulated telemetry."""

from __future__ import annotations

import asyncio

import pytest

from ados.services.mavlink.demo import DemoFCConnection
from ados.services.mavlink.state import VehicleState


def test_demo_connection_properties():
    """DemoFCConnection reports demo connection info."""
    state = VehicleState()
    demo = DemoFCConnection(state)
    assert demo.connected is True
    assert demo.port == "demo"
    assert demo.baud == 0
    assert demo.connection is None


def test_demo_subscribe_unsubscribe():
    """subscribe/unsubscribe manage queues."""
    state = VehicleState()
    demo = DemoFCConnection(state)
    q = demo.subscribe()
    assert isinstance(q, asyncio.Queue)
    assert len(demo._subscribers) == 1
    demo.unsubscribe(q)
    assert len(demo._subscribers) == 0


def test_demo_send_noop():
    """send_bytes and send_heartbeat should not raise."""
    state = VehicleState()
    demo = DemoFCConnection(state)
    demo.send_bytes(b"\x00\x01\x02")
    demo.send_heartbeat()


@pytest.mark.asyncio
async def test_demo_generates_telemetry():
    """Running demo for a short period should update VehicleState."""
    state = VehicleState()
    demo = DemoFCConnection(state)

    # Run demo for 0.3 seconds then cancel
    task = asyncio.create_task(demo.run())
    await asyncio.sleep(0.35)
    task.cancel()
    try:
        await task
    except asyncio.CancelledError:
        pass

    # State should have been updated with Bangalore coordinates
    assert state.lat != 0.0
    assert abs(state.lat - 12.9716) < 0.01
    assert abs(state.lon - 77.5946) < 0.01
    assert state.alt_rel > 40.0  # ~50m ± 3m
    assert state.armed is True
    assert state.mode == "LOITER"
    assert state.gps_fix_type == 3
    assert state.gps_satellites == 14
    assert state.battery_remaining > 0
    assert state.voltage_battery > 20.0
    assert state.last_heartbeat != ""
    assert state.last_update != ""
    assert state.groundspeed > 0.0


@pytest.mark.asyncio
async def test_demo_state_changes_over_time():
    """State should change between updates (drone is moving)."""
    state = VehicleState()
    demo = DemoFCConnection(state)

    task = asyncio.create_task(demo.run())
    await asyncio.sleep(0.15)
    lat1 = state.lat
    lon1 = state.lon
    await asyncio.sleep(0.25)
    lat2 = state.lat
    lon2 = state.lon
    task.cancel()
    try:
        await task
    except asyncio.CancelledError:
        pass

    # Position should have changed (drone flying in circle)
    assert (lat1 != lat2) or (lon1 != lon2)
