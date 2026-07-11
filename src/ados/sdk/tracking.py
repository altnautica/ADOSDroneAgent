"""Shared locked-target behaviour primitives for target-driven plugins.

A plugin that follows the vision engine's operator-designated target — Follow-Me,
the gimbal aim loop, a future ActiveTrack — subscribes to the detection stream and
must apply one safety gate:

* adopt only the engine-designated + tracked target, never pick its own;
* stop commanding the instant the tracker reports the subject uncertain or lost;
* coast briefly through a missed frame rather than twitch;
* never silently re-lock — only an explicit operator re-designate re-locks.

This module owns that gate ONCE so every behaviour shares one audited
implementation instead of hand-rolling it (and drifting: two hand-rolled copies
already disagreed on the coast window). The engine's single-object tracker stamps
exactly one detection per camera with a ``track_id`` + ``lock_state``; the
operator designates through the engine, so the engine owns the lock and its
"never silently re-lock" guarantee carries through here.

The tracker is pure and I/O-free: feed it a detection batch with :meth:`record`
and ask it for the effective lock at any monotonic time with
:meth:`effective_lock` / :meth:`locked_target`. A behaviour drives it from its own
loop (Follow-Me ticks at a fixed rate; the gimbal evaluates per batch) — the coast
window is anchored on the drone's monotonic clock, so it holds regardless of the
driving cadence.
"""

from __future__ import annotations

import enum
from dataclasses import dataclass
from typing import Any

# The engine stamps one of these on a tracked detection's ``lock_state``.
LOCK_LOCKED = "locked"
LOCK_UNCERTAIN = "uncertain"
LOCK_LOST = "lost"

# How long the gate coasts on the last good sighting before declaring the
# subject lost when its detection stops arriving.
DEFAULT_COAST_WINDOW_S = 1.5


class EffectiveLock(str, enum.Enum):
    """The gate's verdict for the current instant.

    ``LOCKED`` is the only state a behaviour commands on. ``UNCERTAIN`` holds
    (stop commanding, keep the lock). ``LOST`` requires the caller to
    :meth:`LockedTargetTracker.drop` and wait for a fresh operator designate.
    ``NONE`` means no target has been adopted at all.
    """

    NONE = "none"
    LOCKED = "locked"
    UNCERTAIN = "uncertain"
    LOST = "lost"

    @property
    def should_command(self) -> bool:
        return self is EffectiveLock.LOCKED


@dataclass
class LockedTarget:
    """The engine-designated target a behaviour may act on this instant."""

    track_id: int
    bbox: Any  # a BoundingBox (x/y/width/height, source-frame pixels)
    lock_state: str  # the raw engine state at the last sighting


class LockedTargetTracker:
    """The locked-target safety gate. Pure, no I/O, unit-testable.

    Records the engine's designated target from each batch and, at any monotonic
    time, reports the effective lock after applying the coast window. Never
    chooses a target itself; never re-locks after a loss without an explicit
    :meth:`drop` + a fresh operator designate on a subsequent batch.
    """

    def __init__(self, coast_window_s: float = DEFAULT_COAST_WINDOW_S) -> None:
        # A negative window must not flip the fail-safe direction (it would make
        # everything instantly lost and silently disable the behaviour); clamp.
        self._coast_s = max(0.0, float(coast_window_s))
        self._track_id: int | None = None
        self._bbox: Any = None
        self._raw_state: str | None = None
        self._last_seen_s: float = 0.0

    @property
    def has_lock(self) -> bool:
        """True once a target has been adopted and not yet dropped."""
        return self._track_id is not None

    @property
    def track_id(self) -> int | None:
        return self._track_id

    def record(self, batch: Any, now_monotonic_s: float) -> None:
        """Adopt the engine's designated target from a detection ``batch``.

        Adopts the first detection carrying both a ``track_id`` and a
        ``lock_state`` (the engine marks exactly one). A batch with no tracked
        detection leaves the last sighting untouched, so the coast window in
        :meth:`effective_lock` decides uncertain-vs-lost. ``now_monotonic_s``
        MUST be the drone's local monotonic clock (e.g. ``time.monotonic()``).
        """
        for det in getattr(batch, "detections", None) or ():
            tid = getattr(det, "track_id", None)
            raw = getattr(det, "lock_state", None)
            if tid is not None and raw is not None:
                self._track_id = int(tid)
                self._bbox = getattr(det, "bbox", None)
                self._raw_state = str(raw)
                self._last_seen_s = now_monotonic_s
                return

    def effective_lock(self, now_monotonic_s: float) -> EffectiveLock:
        """The gate verdict at ``now_monotonic_s`` after the coast window."""
        if self._track_id is None:
            return EffectiveLock.NONE
        # A tracker-reported loss is authoritative and immediate.
        if self._raw_state == LOCK_LOST:
            return EffectiveLock.LOST
        # An adopted target not seen within the coast window is lost.
        if (now_monotonic_s - self._last_seen_s) > self._coast_s:
            return EffectiveLock.LOST
        if self._raw_state == LOCK_UNCERTAIN:
            return EffectiveLock.UNCERTAIN
        return EffectiveLock.LOCKED

    def locked_target(self, now_monotonic_s: float) -> LockedTarget | None:
        """The target to command this instant, or ``None`` to hold.

        Returns a :class:`LockedTarget` only when the effective lock is
        ``LOCKED`` and a bounding box is present; every other state holds.
        """
        if (
            self.effective_lock(now_monotonic_s) is EffectiveLock.LOCKED
            and self._bbox is not None
            and self._track_id is not None
        ):
            return LockedTarget(
                track_id=self._track_id,
                bbox=self._bbox,
                lock_state=self._raw_state or LOCK_LOCKED,
            )
        return None

    def drop(self) -> None:
        """Release the lock. Required on ``LOST``; a fresh operator designate
        (a subsequent :meth:`record` with a tracked detection) re-locks."""
        self._track_id = None
        self._bbox = None
        self._raw_state = None


__all__ = [
    "LOCK_LOCKED",
    "LOCK_UNCERTAIN",
    "LOCK_LOST",
    "DEFAULT_COAST_WINDOW_S",
    "EffectiveLock",
    "LockedTarget",
    "LockedTargetTracker",
]
