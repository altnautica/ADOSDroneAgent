"""Tests for the EWMA-smoothed FPS counter on the local video tap.

The new-sample callback bumps an integer counter; a 1 Hz tick folds the
counter into an EWMA and resets it. The renderer reads the smoothed
value via ``stats()["fps"]``. We simulate frames at fixed intervals by
patching ``time.monotonic`` so tests can advance the clock deterministically.
"""

from __future__ import annotations

import pytest

from ados.services.video import local_tap as lt


class _FakeClock:
    def __init__(self, start: float = 1000.0) -> None:
        self.now = start

    def __call__(self) -> float:
        return self.now

    def advance(self, delta: float) -> None:
        self.now += delta


@pytest.fixture
def clock(monkeypatch: pytest.MonkeyPatch) -> _FakeClock:
    fake = _FakeClock()
    monkeypatch.setattr(lt.time, "monotonic", fake)
    return fake


def _bump(tap: lt.LocalVideoTap) -> None:
    """Simulate a single new-sample call without going through gstreamer."""
    import time as _t

    now = _t.monotonic()
    tap._fps_tick_count += 1
    if tap._fps_tick_at is None:
        tap._fps_tick_at = now
    tap._frames_decoded += 1
    tap._last_frame_at = now


def test_fps_seeds_to_zero(clock: _FakeClock) -> None:
    tap = lt.LocalVideoTap()
    # No samples yet — the first stats() call seeds the tick clock.
    assert tap.stats()["fps"] == 0.0


def test_fps_converges_to_thirty_at_thirty_fps(clock: _FakeClock) -> None:
    tap = lt.LocalVideoTap()
    # Seed.
    tap.stats()
    # Drive the simulator at exactly 30 fps for 5 ticks (5 seconds).
    for _ in range(5):
        for _frame in range(30):
            _bump(tap)
            clock.advance(1.0 / 30.0)
        # Tiny slack avoids float rounding at the 1.0 s boundary.
        clock.advance(0.01)
        # The 1-Hz boundary triggers the EWMA fold on the next stats() call.
        tap.stats()
    fps = tap.stats()["fps"]
    # 30 fps with EWMA alpha=0.2 should converge within ±2 fps in 5 s.
    assert 28.0 <= fps <= 32.0


def test_fps_resets_to_zero_on_stop(clock: _FakeClock) -> None:
    tap = lt.LocalVideoTap()
    tap.stats()
    for _ in range(30):
        _bump(tap)
        clock.advance(1.0 / 30.0)
    # Add a small slack to guarantee the 1 Hz boundary is crossed.
    clock.advance(0.05)
    tap.stats()
    assert tap.stats()["fps"] > 0
    # _reset_stats is called by stop() but stop() needs a pipeline; the
    # public reset path is direct and is the same the stop() helper uses.
    tap._reset_stats()
    assert tap._fps_ewma == 0.0
    assert tap._fps_tick_count == 0
    assert tap._fps_tick_at is None
    # Subsequent stats() seeds again without emitting a stale value.
    assert tap.stats()["fps"] == 0.0


def test_fps_counter_does_not_emit_below_one_second_window(
    clock: _FakeClock,
) -> None:
    tap = lt.LocalVideoTap()
    tap.stats()  # seed
    # 30 frames in the first 0.5 s should not emit a 60 fps spike.
    for _ in range(15):
        _bump(tap)
        clock.advance(1.0 / 30.0)
    # Less than 1 s elapsed since the seed, so EWMA still 0.
    fps = tap.stats()["fps"]
    assert fps == 0.0


def test_fps_handles_starvation(clock: _FakeClock) -> None:
    tap = lt.LocalVideoTap()
    tap.stats()  # seed
    # Push one batch of frames then stop bumping for 2 s.
    for _ in range(30):
        _bump(tap)
        clock.advance(1.0 / 30.0)
    tap.stats()  # fold first window
    clock.advance(2.0)
    # Now zero frames in the last window: fps should fall (EWMA towards 0).
    fps = tap.stats()["fps"]
    assert fps < 30.0
