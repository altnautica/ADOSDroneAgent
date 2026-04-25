"""Tests for ground-station uplink router, event bus, adapters, and data cap.

Regression net before any decomposition of the file. Covers the natural
class boundaries: UplinkEvent + UplinkEventBus, the three manager
adapters, _UsageState + DataCapTracker, and high-level UplinkRouter
selection / failover behaviour.
"""

from __future__ import annotations

import time
from pathlib import Path
from typing import Any, Optional
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from ados.services.ground_station.uplink_router import (
    DataCapTracker,
    UplinkEvent,
    UplinkEventBus,
    UplinkRouter,
    _EthernetAdapter,
    _ModemAdapter,
    _UsageState,
    _WifiClientAdapter,
)


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


def _make_event(kind: str = "uplink_changed", active: Optional[str] = "eth0") -> UplinkEvent:
    return UplinkEvent(
        kind=kind,  # type: ignore[arg-type]
        active_uplink=active,
        available=[active] if active else [],
        internet_reachable=True,
        data_cap_state="ok",
        timestamp_ms=int(time.time() * 1000),
    )


# ---------------------------------------------------------------------------
# UplinkEventBus
# ---------------------------------------------------------------------------


async def test_event_bus_publish_to_single_subscriber():
    bus = UplinkEventBus()
    received: list[UplinkEvent] = []

    import asyncio

    async def consume():
        async for evt in bus.subscribe():
            received.append(evt)
            if len(received) >= 2:
                return

    task = asyncio.create_task(consume())
    await asyncio.sleep(0.01)
    await bus.publish(_make_event("uplink_changed", "eth0"))
    await bus.publish(_make_event("health_changed", "wlan0_client"))
    await asyncio.wait_for(task, timeout=1.0)

    assert len(received) == 2
    assert received[0].kind == "uplink_changed"
    assert received[0].active_uplink == "eth0"
    assert received[1].kind == "health_changed"
    assert received[1].active_uplink == "wlan0_client"

    await bus.close()


async def test_event_bus_fanout_multiple_subscribers():
    bus = UplinkEventBus()
    a: list[UplinkEvent] = []
    b: list[UplinkEvent] = []

    import asyncio

    async def consume(sink: list[UplinkEvent]):
        async for evt in bus.subscribe():
            sink.append(evt)
            if len(sink) >= 1:
                return

    t_a = asyncio.create_task(consume(a))
    t_b = asyncio.create_task(consume(b))
    await asyncio.sleep(0.01)

    await bus.publish(_make_event())
    await asyncio.wait_for(asyncio.gather(t_a, t_b), timeout=1.0)

    assert len(a) == 1
    assert len(b) == 1
    assert a[0].active_uplink == b[0].active_uplink

    await bus.close()


async def test_event_bus_close_terminates_subscribers():
    bus = UplinkEventBus()
    import asyncio

    async def consume():
        count = 0
        async for _evt in bus.subscribe():
            count += 1
        return count

    task = asyncio.create_task(consume())
    await asyncio.sleep(0.01)
    await bus.close()
    result = await asyncio.wait_for(task, timeout=1.0)
    assert result == 0


# ---------------------------------------------------------------------------
# Manager adapters
# ---------------------------------------------------------------------------


async def test_modem_adapter_is_up_and_iface():
    modem = MagicMock()
    modem.status = AsyncMock(return_value={"state": "connected", "ip": "10.0.0.5"})
    modem._current_iface = MagicMock(return_value="wwan0")

    adapter = _ModemAdapter(modem)

    assert await adapter.is_up() is True
    assert adapter.get_iface() == "wwan0"


async def test_modem_adapter_is_up_disconnected():
    modem = MagicMock()
    modem.status = AsyncMock(return_value={"state": "idle", "ip": None})

    adapter = _ModemAdapter(modem)

    assert await adapter.is_up() is False


async def test_modem_adapter_status_failure_returns_false():
    modem = MagicMock()
    modem.status = AsyncMock(side_effect=RuntimeError("modem offline"))

    adapter = _ModemAdapter(modem)
    assert await adapter.is_up() is False


async def test_wifi_adapter_status_surfaces():
    wifi = MagicMock()
    wifi.status = AsyncMock(
        return_value={"connected": True, "ip": "192.168.1.10", "gateway": "192.168.1.1"}
    )
    wifi._interface = "wlan0"

    adapter = _WifiClientAdapter(wifi)

    assert await adapter.is_up() is True
    assert adapter.get_iface() == "wlan0"
    assert await adapter.get_gateway() == "192.168.1.1"


async def test_wifi_adapter_disconnected():
    wifi = MagicMock()
    wifi.status = AsyncMock(return_value={"connected": False, "ip": None, "gateway": None})

    adapter = _WifiClientAdapter(wifi)

    assert await adapter.is_up() is False
    assert await adapter.get_gateway() is None


async def test_ethernet_adapter_status_surfaces():
    eth = MagicMock()
    eth.status = AsyncMock(
        return_value={"link": True, "ip": "10.10.0.5", "gateway": "10.10.0.1"}
    )
    eth._interface = "eth0"

    adapter = _EthernetAdapter(eth)

    assert await adapter.is_up() is True
    assert adapter.get_iface() == "eth0"
    assert await adapter.get_gateway() == "10.10.0.1"


async def test_ethernet_adapter_no_link():
    eth = MagicMock()
    eth.status = AsyncMock(return_value={"link": False, "ip": None})

    adapter = _EthernetAdapter(eth)

    assert await adapter.is_up() is False


# ---------------------------------------------------------------------------
# _UsageState (de)serialization
# ---------------------------------------------------------------------------


def test_usage_state_round_trip_json():
    state = _UsageState(
        window_started_at=1700000000.0,
        cumulative_bytes=12345,
        last_rx=100,
        last_tx=200,
        last_reset_month="2026-04",
    )
    data = state.to_json()
    restored = _UsageState.from_json(data)

    assert restored.window_started_at == 1700000000.0
    assert restored.cumulative_bytes == 12345
    assert restored.last_rx == 100
    assert restored.last_tx == 200
    assert restored.last_reset_month == "2026-04"


def test_usage_state_from_json_handles_missing_fields():
    restored = _UsageState.from_json({})
    assert restored.cumulative_bytes == 0
    assert restored.last_rx == 0
    assert restored.last_tx == 0
    assert restored.last_reset_month == ""


# ---------------------------------------------------------------------------
# DataCapTracker
# ---------------------------------------------------------------------------


def _make_tracker(tmp_path: Path, cap_gb: float = 1.0) -> tuple[DataCapTracker, MagicMock, UplinkEventBus]:
    modem = MagicMock()
    modem.data_usage = AsyncMock(return_value={"rx_bytes": 0, "tx_bytes": 0})
    bus = UplinkEventBus()
    state_path = tmp_path / "modem-usage.json"
    tracker = DataCapTracker(modem, bus, cap_gb=cap_gb, state_path=state_path)
    return tracker, modem, bus


async def test_data_cap_accumulates_bytes(tmp_path: Path):
    tracker, modem, _bus = _make_tracker(tmp_path, cap_gb=1.0)

    # First poll: baseline.
    modem.data_usage = AsyncMock(return_value={"rx_bytes": 1000, "tx_bytes": 500})
    await tracker._poll_once()
    # First baseline pass: state.last_rx and state.last_tx are zero, so
    # the diff is 1500. Use fresh tracker semantics: confirm the
    # cumulative byte count tracks the diff against the previous sample.
    assert tracker._state.cumulative_bytes == 1500

    # Second poll adds 700 + 300 = 1000 more.
    modem.data_usage = AsyncMock(return_value={"rx_bytes": 1700, "tx_bytes": 800})
    await tracker._poll_once()
    assert tracker._state.cumulative_bytes == 2500


async def test_data_cap_modem_counter_reset(tmp_path: Path):
    tracker, modem, _bus = _make_tracker(tmp_path, cap_gb=1.0)
    tracker._state.last_rx = 5000
    tracker._state.last_tx = 3000
    tracker._state.cumulative_bytes = 8000

    # New sample is smaller than last, signalling a reboot reset.
    modem.data_usage = AsyncMock(return_value={"rx_bytes": 100, "tx_bytes": 50})
    await tracker._poll_once()

    # Diff is suppressed; cumulative stays put, baseline updates.
    assert tracker._state.cumulative_bytes == 8000
    assert tracker._state.last_rx == 100
    assert tracker._state.last_tx == 50


async def test_data_cap_warn_threshold_at_80_percent(tmp_path: Path):
    tracker, modem, bus = _make_tracker(tmp_path, cap_gb=1.0)
    cap = tracker._cap_bytes
    threshold_80 = int(cap * 0.81)

    received: list[UplinkEvent] = []
    import asyncio

    async def consume():
        async for evt in bus.subscribe():
            received.append(evt)
            if received:
                return

    task = asyncio.create_task(consume())
    await asyncio.sleep(0.01)

    modem.data_usage = AsyncMock(return_value={"rx_bytes": threshold_80, "tx_bytes": 0})
    await tracker._poll_once()
    await asyncio.wait_for(task, timeout=1.0)

    assert received[0].kind == "data_cap_threshold"
    assert received[0].data_cap_state == "warn_80"

    await bus.close()


async def test_data_cap_throttle_threshold_at_95_percent(tmp_path: Path):
    tracker, modem, _bus = _make_tracker(tmp_path, cap_gb=1.0)
    cap = tracker._cap_bytes
    over_95 = int(cap * 0.96)

    modem.data_usage = AsyncMock(return_value={"rx_bytes": over_95, "tx_bytes": 0})
    await tracker._poll_once()

    assert tracker._classify() == "throttle_95"


async def test_data_cap_blocked_threshold_at_100_percent(tmp_path: Path):
    tracker, modem, _bus = _make_tracker(tmp_path, cap_gb=1.0)
    cap = tracker._cap_bytes

    modem.data_usage = AsyncMock(return_value={"rx_bytes": cap + 1024, "tx_bytes": 0})
    await tracker._poll_once()

    assert tracker._classify() == "blocked_100"


def test_data_cap_classify_below_threshold(tmp_path: Path):
    tracker, _modem, _bus = _make_tracker(tmp_path, cap_gb=1.0)
    tracker._state.cumulative_bytes = 100
    assert tracker._classify() == "ok"


def test_data_cap_set_cap_updates_bytes(tmp_path: Path):
    tracker, _modem, _bus = _make_tracker(tmp_path, cap_gb=1.0)
    tracker.set_cap(2.0)
    assert tracker._cap_bytes == int(2.0 * 1024 * 1024 * 1024)


def test_data_cap_get_usage_payload(tmp_path: Path):
    tracker, _modem, _bus = _make_tracker(tmp_path, cap_gb=1.0)
    tracker._state.cumulative_bytes = 100 * 1024 * 1024  # 100 MB

    usage = tracker.get_usage()
    assert usage["data_used_mb"] == 100
    assert usage["cap_mb"] == 1024
    assert 9.0 < usage["percent"] < 11.0
    assert usage["state"] == "ok"


# ---------------------------------------------------------------------------
# UplinkRouter
# ---------------------------------------------------------------------------


class _FakeManager:
    """Lightweight test double matching the uplink manager protocol."""

    def __init__(self, iface: str, up: bool = True, gateway: Optional[str] = "10.0.0.1") -> None:
        self._iface = iface
        self._up = up
        self._gateway = gateway

    async def is_up(self) -> bool:
        return self._up

    def get_iface(self) -> str:
        return self._iface

    async def get_gateway(self) -> Optional[str]:
        return self._gateway


def _make_router(
    tmp_path: Path,
    eth_up: bool = True,
    wifi_up: bool = True,
    modem_up: bool = False,
) -> UplinkRouter:
    return UplinkRouter(
        modem_manager=_FakeManager("wwan0", up=modem_up, gateway="10.20.30.1"),
        wifi_client_manager=_FakeManager("wlan0", up=wifi_up, gateway="192.168.1.1"),
        ethernet_manager=_FakeManager("eth0", up=eth_up, gateway="10.0.0.1"),
        usb_tether_check=AsyncMock(return_value=False),
        priority_config_path=tmp_path / "priority.json",
    )


async def test_router_initial_pick_highest_priority(tmp_path: Path):
    router = _make_router(tmp_path, eth_up=True, wifi_up=True)

    with patch.object(router, "_apply_default_route", return_value=True), patch.object(
        router, "_probe_host", AsyncMock(return_value=True)
    ):
        await router._tick()

    assert router.active_uplink == "eth0"
    assert router.internet_reachable is True


async def test_router_initial_pick_skips_unviable(tmp_path: Path):
    router = _make_router(tmp_path, eth_up=False, wifi_up=True)

    with patch.object(router, "_apply_default_route", return_value=True), patch.object(
        router, "_probe_host", AsyncMock(return_value=True)
    ):
        await router._tick()

    # eth0 is down, so wlan0_client takes over.
    assert router.active_uplink == "wlan0_client"


async def test_router_no_viable_uplinks_clears_state(tmp_path: Path):
    router = _make_router(tmp_path, eth_up=False, wifi_up=False, modem_up=False)
    router.active_uplink = "eth0"
    router.internet_reachable = True

    with patch.object(router, "_apply_default_route", return_value=True), patch.object(
        router, "_probe_host", AsyncMock(return_value=False)
    ):
        await router._tick()

    assert router.active_uplink is None
    assert router.internet_reachable is False


def test_router_set_priority_validates(tmp_path: Path):
    router = _make_router(tmp_path)

    with pytest.raises(ValueError):
        router.set_priority([])

    with pytest.raises(ValueError):
        router.set_priority([1, 2])  # type: ignore[list-item]


def test_router_set_priority_persists(tmp_path: Path):
    router = _make_router(tmp_path)
    router.set_priority(["wlan0_client", "eth0"])

    assert router.get_priority() == ["wlan0_client", "eth0"]
    saved = (tmp_path / "priority.json").read_text(encoding="utf-8")
    assert "wlan0_client" in saved
    assert "eth0" in saved


def test_router_get_state_shape(tmp_path: Path):
    router = _make_router(tmp_path, modem_up=True)
    state = router.get_state()

    assert "active_uplink" in state
    assert "internet_reachable" in state
    assert "priority" in state
    assert "fail_streak" in state
    assert "success_streak" in state
    assert isinstance(state["priority"], list)


async def test_router_failover_after_fail_streak(tmp_path: Path):
    """Probe failures past the threshold trigger failover to the next uplink."""

    router = _make_router(tmp_path, eth_up=True, wifi_up=True)

    # Force enough elapsed time so cooldown is satisfied.
    router._last_switch_at = time.monotonic() - 60.0

    with patch.object(router, "_apply_default_route", return_value=True), patch.object(
        router, "_probe_host", AsyncMock(return_value=False)
    ):
        # Establish active uplink first via a successful viability check
        # but failing probes. With no prior active_uplink, the first
        # tick picks eth0 then probes fail. Each tick increments the
        # fail streak. Three ticks at fail threshold = failover.
        router.active_uplink = "eth0"
        router._last_switch_at = time.monotonic() - 60.0

        for _ in range(3):
            await router._tick()

    assert router.active_uplink == "wlan0_client"
