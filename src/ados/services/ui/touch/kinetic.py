"""Kinetic decay state machine for drag-scroll.

When the operator releases a drag with non-trivial velocity, pages
that scroll (Settings list, MAVLink log) keep gliding by feeding the
release velocity into a :class:`KineticDecay`. Each render tick the
page calls :meth:`tick` with the elapsed seconds; the decay returns
the pixel offset to add to its scroll position. When the velocity
drops below the stop threshold the state machine reports zero and
the page stops requesting redraws.

The decay rate is tuned to match the mockups: ~8% velocity loss per
50 ms, so a 1000 px/s release coasts roughly 100 px before stopping.
"""

from __future__ import annotations

# Per-tick decay factor at the reference 50 ms (20 Hz) frame interval.
# velocity *= 0.92 every 50 ms means after one second the velocity is
# 0.92^20 ≈ 0.19 of the initial — a feel that matches "scroll, slow,
# stop" without overshooting.
_DECAY_PER_50MS = 0.92

# Below this absolute velocity, the decay reports stop. 10 px/s is one
# pixel every 100 ms; below that a render tick can't show motion.
_STOP_THRESHOLD = 10.0


class KineticDecay:
    """Tracks a single decaying scroll velocity over time."""

    def __init__(self) -> None:
        self._velocity_px_per_s: float = 0.0
        self._active: bool = False

    def start(self, velocity_px_per_s: float) -> None:
        """Begin a decay with the given release velocity."""
        self._velocity_px_per_s = float(velocity_px_per_s)
        self._active = abs(self._velocity_px_per_s) >= _STOP_THRESHOLD

    def stop(self) -> None:
        """Halt the decay immediately."""
        self._velocity_px_per_s = 0.0
        self._active = False

    @property
    def active(self) -> bool:
        return self._active

    @property
    def velocity_px_per_s(self) -> float:
        return self._velocity_px_per_s

    def tick(self, dt_seconds: float) -> float:
        """Advance the decay by ``dt_seconds`` and return the offset.

        The offset is the number of pixels the scroll position should
        change this tick. Positive or negative depending on the
        direction of the seeded velocity. After this call, the
        velocity has been multiplied by ``decay_per_50ms ** (dt_s /
        0.050)``; calling ``tick`` after the velocity falls below the
        stop threshold returns 0 and flips ``active`` to False.
        """
        if not self._active:
            return 0.0
        if dt_seconds <= 0.0:
            return 0.0
        offset = self._velocity_px_per_s * dt_seconds
        # Decay factor scaled to the actual elapsed time.
        # decay = (decay_per_50ms) ** (dt / 0.050)
        ticks = dt_seconds / 0.050
        self._velocity_px_per_s *= _DECAY_PER_50MS ** ticks
        if abs(self._velocity_px_per_s) < _STOP_THRESHOLD:
            self._active = False
            self._velocity_px_per_s = 0.0
        return offset
