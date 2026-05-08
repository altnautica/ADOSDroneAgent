"""Tests for the touch input bridge.

Covers:

* Affine + identity transform produce expected LCD coordinates.
* Stroke classification (tap, long_press, swipe, drag, direction).
* Move-bus delta filtering.
* Gesture-bus emission on pen-up.
* Calibration reload after a wizard run.
"""

from __future__ import annotations

import asyncio
from pathlib import Path

import pytest

from ados.services.ui.touch.bridge import TouchInputBridge
from ados.services.ui.touch.events import TouchEventBus, TouchMoveBus
from ados.services.ui.touch.transform import (
    Affine,
    compute_from_samples,
    identity_for,
    load,
    save,
    save_skip_marker,
)


# ---------------------------------------------------------------------------
# Affine math (deterministic, no I/O)
# ---------------------------------------------------------------------------


class TestAffineMath:
    def test_identity_for_rotation_zero(self):
        a = identity_for(0, (480, 320))
        assert a.apply(0, 0) == (0, 0)
        assert a.apply(4095, 4095) == (480, 320)

    def test_identity_for_unknown_rotation_falls_back_to_zero(self):
        a = identity_for(45, (480, 320))
        # Falls back to the rotation=0 mapping.
        assert a.apply(0, 0) == (0, 0)

    def test_compute_from_samples_perfect_fit(self):
        samples = [
            (100, 100), (3995, 100), (2047, 2047),
            (100, 3995), (3995, 3995),
        ]
        targets = [
            (40, 40), (440, 40), (240, 160),
            (40, 280), (440, 280),
        ]
        affine, rms = compute_from_samples(samples, targets)
        assert rms < 5.0, f"rms too high: {rms}"
        # Points used for the fit should round-trip near-exactly.
        for (xr, yr), (xt, yt) in zip(samples, targets):
            x_lcd, y_lcd = affine.apply(xr, yr)
            assert abs(x_lcd - xt) <= 1
            assert abs(y_lcd - yt) <= 1

    def test_compute_from_samples_rejects_short_input(self):
        with pytest.raises(ValueError):
            compute_from_samples([(0, 0)], [(0, 0)])

    def test_compute_from_samples_rejects_mismatched_lengths(self):
        with pytest.raises(ValueError):
            compute_from_samples(
                [(0, 0)] * 5, [(0, 0)] * 4,
            )

    def test_compute_from_samples_singular_inputs_raise(self):
        # All raw samples on the same x — the normal-equations matrix
        # is rank deficient.
        samples = [(100, 0), (100, 1000), (100, 2000), (100, 3000), (100, 4095)]
        targets = [(40, 40), (40, 80), (40, 120), (40, 160), (40, 200)]
        with pytest.raises(ValueError):
            compute_from_samples(samples, targets)


# ---------------------------------------------------------------------------
# Persistence (round trip + skip marker)
# ---------------------------------------------------------------------------


class TestCalibrationPersistence:
    def test_save_and_load_round_trip(self, tmp_path: Path):
        a = Affine(a=0.1, b=0.0, c=5.0, d=0.0, e=0.1, f=2.0)
        target = tmp_path / "touch.calib"
        save(a, target, rotation=0, rms=1.2)
        loaded = load(target)
        assert loaded is not None
        assert abs(loaded.a - a.a) < 1e-9
        assert abs(loaded.f - a.f) < 1e-9

    def test_skip_marker_loads_as_none(self, tmp_path: Path):
        target = tmp_path / "touch.calib"
        save_skip_marker(target)
        assert load(target) is None

    def test_missing_file_loads_as_none(self, tmp_path: Path):
        assert load(tmp_path / "absent.calib") is None

    def test_corrupt_json_loads_as_none(self, tmp_path: Path):
        target = tmp_path / "touch.calib"
        target.write_text("{not json")
        assert load(target) is None


# ---------------------------------------------------------------------------
# Bridge classifier — drives the bridge against a synthetic evdev stream.
# ---------------------------------------------------------------------------


class _FakeEvent:
    """A minimal stand-in for evdev's InputEvent."""

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


@pytest.mark.asyncio
async def test_bridge_emits_tap_for_short_stationary_stroke(monkeypatch):
    bus = TouchEventBus()
    move_bus = TouchMoveBus()
    bridge = TouchInputBridge(
        legacy_button_bus=None,
        rotation=0,
        lcd_size=(480, 320),
        gesture_bus=bus,
        move_bus=move_bus,
    )
    bridge.mode = "lcd_page"

    # Build a "tap" sequence at raw (2000, 2000): pen down, ABS pair,
    # SYN, pen up.
    e = _FakeECodes
    events = [
        _FakeEvent(e.EV_KEY, e.BTN_TOUCH, 1),
        _FakeEvent(e.EV_ABS, e.ABS_X, 2000),
        _FakeEvent(e.EV_ABS, e.ABS_Y, 2000),
        _FakeEvent(e.EV_SYN, 0, 0),
        _FakeEvent(e.EV_KEY, e.BTN_TOUCH, 0),
    ]
    device = _FakeDevice(events)

    received: list = []

    async def collect():
        async for g in bus.subscribe():
            received.append(g)
            return

    collector = asyncio.create_task(collect())
    await asyncio.sleep(0)
    await bridge._consume(device, e)
    # Allow the sentinel-less subscriber to receive the gesture.
    await asyncio.sleep(0.05)
    await bus.close()
    await collector

    assert len(received) == 1
    g = received[0]
    assert g.kind == "tap"
    assert g.direction is None
    # Identity 0..4095 -> 0..480: 2000 * 480/4095 ≈ 234, y ≈ 156.
    assert 200 <= g.start_x <= 260
    assert 130 <= g.start_y <= 180


@pytest.mark.asyncio
async def test_bridge_emits_swipe_for_fast_horizontal_stroke():
    bus = TouchEventBus()
    move_bus = TouchMoveBus()
    bridge = TouchInputBridge(
        gesture_bus=bus, move_bus=move_bus,
    )
    bridge.mode = "lcd_page"

    e = _FakeECodes
    events = [
        _FakeEvent(e.EV_KEY, e.BTN_TOUCH, 1),
        _FakeEvent(e.EV_ABS, e.ABS_X, 500),
        _FakeEvent(e.EV_ABS, e.ABS_Y, 2000),
        _FakeEvent(e.EV_SYN, 0, 0),
        _FakeEvent(e.EV_ABS, e.ABS_X, 3500),
        _FakeEvent(e.EV_ABS, e.ABS_Y, 2000),
        _FakeEvent(e.EV_SYN, 0, 0),
        _FakeEvent(e.EV_KEY, e.BTN_TOUCH, 0),
    ]
    device = _FakeDevice(events)
    received: list = []

    async def collect():
        async for g in bus.subscribe():
            received.append(g)
            return

    collector = asyncio.create_task(collect())
    await asyncio.sleep(0)
    await bridge._consume(device, e)
    await asyncio.sleep(0.05)
    await bus.close()
    await collector

    assert len(received) == 1
    g = received[0]
    # Large horizontal displacement -> swipe right.
    assert g.kind == "swipe"
    assert g.direction == "right"


@pytest.mark.asyncio
async def test_bridge_emits_long_press_for_held_stationary_stroke(monkeypatch):
    """A pen held for >= 400 ms with no motion classifies as long_press."""
    from ados.services.ui.touch import bridge as bridge_mod

    fake_clock = {"ms": 1000}

    def fake_now_ms():
        return fake_clock["ms"]

    monkeypatch.setattr(bridge_mod, "_now_ms", fake_now_ms)

    bus = TouchEventBus()
    move_bus = TouchMoveBus()
    b = bridge_mod.TouchInputBridge(
        gesture_bus=bus, move_bus=move_bus,
    )
    b.mode = "lcd_page"

    # Open stroke at 1000 ms.
    b._open_stroke(fake_clock["ms"])
    # First sample lands.
    fake_clock["ms"] = 1010
    await b._record_move(2000, 2000, fake_clock["ms"])
    # Hold for 600 ms with no move.
    fake_clock["ms"] = 1610
    received: list = []

    async def collect():
        async for g in bus.subscribe():
            received.append(g)
            return

    collector = asyncio.create_task(collect())
    await asyncio.sleep(0)
    await b._close_stroke(fake_clock["ms"])
    await asyncio.sleep(0.05)
    await bus.close()
    await collector

    assert len(received) == 1
    assert received[0].kind == "long_press"


@pytest.mark.asyncio
async def test_bridge_legacy_button_bus_publishes_tap_in_oled_compat():
    from ados.services.ui.events import ButtonEvent, ButtonEventBus

    legacy = ButtonEventBus()
    bus = TouchEventBus()
    move_bus = TouchMoveBus()
    bridge = TouchInputBridge(
        legacy_button_bus=legacy,
        gesture_bus=bus,
        move_bus=move_bus,
    )
    bridge.mode = "oled_compat"

    received: list[ButtonEvent] = []

    async def collect():
        async for ev in legacy.subscribe():
            received.append(ev)
            return

    collector = asyncio.create_task(collect())
    await asyncio.sleep(0)

    e = _FakeECodes
    events = [
        _FakeEvent(e.EV_KEY, e.BTN_TOUCH, 1),
        _FakeEvent(e.EV_ABS, e.ABS_X, 2000),
        _FakeEvent(e.EV_ABS, e.ABS_Y, 2000),
        _FakeEvent(e.EV_SYN, 0, 0),
        _FakeEvent(e.EV_KEY, e.BTN_TOUCH, 0),
    ]
    await bridge._consume(_FakeDevice(events), e)
    await asyncio.sleep(0.05)
    await legacy.close()
    await collector

    assert len(received) == 1
    assert received[0].button == 6
    assert received[0].kind == "short"


def test_bridge_mode_setter_rejects_unknown():
    b = TouchInputBridge()
    with pytest.raises(ValueError):
        b.mode = "garbage"  # type: ignore[assignment]


def test_bridge_calibration_reload_after_save(tmp_path: Path):
    """After save() lands on disk, reload_calibration() picks it up."""
    calib = tmp_path / "touch.calib"
    b = TouchInputBridge(
        rotation=0,
        lcd_size=(480, 320),
        calib_path=calib,
    )
    initial = b._affine
    a = Affine(a=2.0, b=0.0, c=10.0, d=0.0, e=2.0, f=20.0)
    save(a, calib, rotation=0, rms=1.0)
    b.reload_calibration()
    assert b._affine != initial
    assert abs(b._affine.a - 2.0) < 1e-9
