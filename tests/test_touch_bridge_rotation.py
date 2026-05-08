"""Tests that the rotation kwarg threads through TouchInputBridge.

The bridge defaults to ``rotation=0`` if the caller does not pass
the kwarg. Without the rotation passed in, on a Waveshare 3.5" RPi
LCD A mounted at 90 degrees taps land 12-16 px off-axis from where
the operator pointed. These tests confirm the rotation kwarg
propagates through to ``identity_for()`` and produces the rotated
identity transform.

We feed a synthetic evdev sequence (BTN_TOUCH=1, ABS_X, ABS_Y, SYN,
BTN_TOUCH=0) directly into the bridge's ``_consume`` loop and assert
on the published TouchGesture's start coordinates.
"""

from __future__ import annotations

import asyncio

import pytest

from ados.services.ui.touch.bridge import TouchInputBridge
from ados.services.ui.touch.events import TouchEventBus, TouchMoveBus
from ados.services.ui.touch.transform import identity_for


class _FakeEvent:
    def __init__(self, etype: int, code: int, value: int) -> None:
        self.type = etype
        self.code = code
        self.value = value


class _FakeECodes:
    EV_KEY = 1
    EV_ABS = 3
    EV_SYN = 0
    BTN_TOUCH = 330
    ABS_X = 0
    ABS_Y = 1


class _FakeDevice:
    """Async-iterable fake that yields a scripted sequence of events."""

    def __init__(self, events: list[_FakeEvent]) -> None:
        self._events = events
        self.path = "/dev/input/event-fake"
        self.name = "ADS7846 Touchscreen"

    def async_read_loop(self):
        events = self._events

        async def gen():
            for ev in events:
                yield ev

        return gen()


def _build_tap_events(raw_x: int, raw_y: int) -> list[_FakeEvent]:
    e = _FakeECodes
    return [
        _FakeEvent(e.EV_KEY, e.BTN_TOUCH, 1),
        _FakeEvent(e.EV_ABS, e.ABS_X, raw_x),
        _FakeEvent(e.EV_ABS, e.ABS_Y, raw_y),
        _FakeEvent(e.EV_SYN, 0, 0),
        _FakeEvent(e.EV_KEY, e.BTN_TOUCH, 0),
    ]


async def _run_bridge_collect_one(
    bridge: TouchInputBridge,
    bus: TouchEventBus,
    events: list[_FakeEvent],
):
    """Drive the bridge over a fake device and return the first gesture."""
    device = _FakeDevice(events)
    received: list = []

    async def collect():
        async for g in bus.subscribe():
            received.append(g)
            return

    collector = asyncio.create_task(collect())
    await asyncio.sleep(0)
    await bridge._consume(device, _FakeECodes)
    await asyncio.sleep(0.05)
    await bus.close()
    await collector
    return received[0] if received else None


# ---------------------------------------------------------------------------
# Per-rotation expected mappings, taken from the canonical
# ``identity_for`` implementation.
# ---------------------------------------------------------------------------


@pytest.mark.parametrize("rotation", [0, 90, 180, 270])
@pytest.mark.asyncio
async def test_bridge_rotation_kwarg_threads_to_transform(rotation):
    """Bridge constructed with rotation=X uses identity_for(X)."""
    bus = TouchEventBus()
    move_bus = TouchMoveBus()
    bridge = TouchInputBridge(
        legacy_button_bus=None,
        rotation=rotation,
        lcd_size=(480, 320),
        gesture_bus=bus,
        move_bus=move_bus,
    )
    bridge.mode = "lcd_page"

    raw_x, raw_y = 4000, 100
    expected = identity_for(rotation, (480, 320)).apply(raw_x, raw_y)

    g = await _run_bridge_collect_one(
        bridge, bus, _build_tap_events(raw_x, raw_y),
    )
    assert g is not None, f"no gesture published for rotation={rotation}"
    # start_x / start_y are the LCD coords seeded from the first ABS
    # pair under the rotated identity transform.
    assert g.start_x == expected[0], (
        f"rotation={rotation}: start_x expected {expected[0]} "
        f"got {g.start_x}"
    )
    assert g.start_y == expected[1], (
        f"rotation={rotation}: start_y expected {expected[1]} "
        f"got {g.start_y}"
    )


@pytest.mark.asyncio
async def test_bridge_rotation_default_is_zero():
    """Construct without passing rotation -> rotation=0 transform applies."""
    bus = TouchEventBus()
    move_bus = TouchMoveBus()
    bridge = TouchInputBridge(
        legacy_button_bus=None,
        gesture_bus=bus,
        move_bus=move_bus,
    )
    bridge.mode = "lcd_page"

    raw_x, raw_y = 4000, 100
    expected = identity_for(0, (480, 320)).apply(raw_x, raw_y)

    g = await _run_bridge_collect_one(
        bridge, bus, _build_tap_events(raw_x, raw_y),
    )
    assert g is not None
    assert g.start_x == expected[0]
    assert g.start_y == expected[1]


# ---------------------------------------------------------------------------
# Concrete numerical assertions documenting the spec's expected values.
# ---------------------------------------------------------------------------


class TestRotationConcreteValues:
    """Concrete values per the AGENT-FIX spec for raw (4000, 100)."""

    @pytest.mark.asyncio
    async def test_rotation_0(self):
        bus = TouchEventBus()
        bridge = TouchInputBridge(
            rotation=0,
            lcd_size=(480, 320),
            gesture_bus=bus,
        )
        bridge.mode = "lcd_page"
        g = await _run_bridge_collect_one(
            bridge, bus, _build_tap_events(4000, 100),
        )
        assert g is not None
        # Spec says ~(469, 8); identity computes (469, 8) exactly.
        assert (g.start_x, g.start_y) == (469, 8)

    @pytest.mark.asyncio
    async def test_rotation_90(self):
        bus = TouchEventBus()
        bridge = TouchInputBridge(
            rotation=90,
            lcd_size=(480, 320),
            gesture_bus=bus,
        )
        bridge.mode = "lcd_page"
        g = await _run_bridge_collect_one(
            bridge, bus, _build_tap_events(4000, 100),
        )
        assert g is not None
        # identity_for(90) maps (4000, 100) -> (12, 7) per the
        # canonical transform.
        assert (g.start_x, g.start_y) == (12, 7)

    @pytest.mark.asyncio
    async def test_rotation_180(self):
        bus = TouchEventBus()
        bridge = TouchInputBridge(
            rotation=180,
            lcd_size=(480, 320),
            gesture_bus=bus,
        )
        bridge.mode = "lcd_page"
        g = await _run_bridge_collect_one(
            bridge, bus, _build_tap_events(4000, 100),
        )
        assert g is not None
        # identity_for(180) maps (4000, 100) -> (11, 312)
        assert (g.start_x, g.start_y) == (11, 312)

    @pytest.mark.asyncio
    async def test_rotation_270(self):
        bus = TouchEventBus()
        bridge = TouchInputBridge(
            rotation=270,
            lcd_size=(480, 320),
            gesture_bus=bus,
        )
        bridge.mode = "lcd_page"
        g = await _run_bridge_collect_one(
            bridge, bus, _build_tap_events(4000, 100),
        )
        assert g is not None
        # identity_for(270) maps (4000, 100) -> (468, 313)
        assert (g.start_x, g.start_y) == (468, 313)
