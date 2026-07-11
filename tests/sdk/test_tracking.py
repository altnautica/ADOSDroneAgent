"""Unit tests for the shared locked-target safety gate.

The canonical behaviour of the gate that Follow-Me + the gimbal aim loop share:
adopt only the engine-designated tracked target, coast briefly, stop on
uncertain/lost, and never re-lock without an explicit re-designate.
"""

from __future__ import annotations

from dataclasses import dataclass, field

from ados.sdk.tracking import (
    DEFAULT_COAST_WINDOW_S,
    LOCK_LOCKED,
    LOCK_LOST,
    LOCK_UNCERTAIN,
    EffectiveLock,
    LockedTargetTracker,
)


@dataclass
class _Box:
    x: float = 0.0
    y: float = 0.0
    width: float = 10.0
    height: float = 10.0


@dataclass
class _Det:
    track_id: int | None = None
    lock_state: str | None = None
    bbox: _Box = field(default_factory=_Box)


@dataclass
class _Batch:
    detections: list[_Det] = field(default_factory=list)


def _tracked(state: str, tid: int = 7) -> _Batch:
    return _Batch(detections=[_Det(track_id=tid, lock_state=state)])


def test_no_target_is_none() -> None:
    t = LockedTargetTracker()
    assert t.has_lock is False
    assert t.effective_lock(0.0) is EffectiveLock.NONE
    assert t.locked_target(0.0) is None


def test_adopts_only_a_tracked_detection() -> None:
    t = LockedTargetTracker()
    # An untracked detection (no track_id / no lock_state) is never adopted.
    t.record(_Batch(detections=[_Det(track_id=None, lock_state=None)]), 0.0)
    assert t.has_lock is False
    t.record(_tracked(LOCK_LOCKED, tid=3), 0.0)
    assert t.has_lock is True
    assert t.track_id == 3


def test_locked_commands_uncertain_holds() -> None:
    t = LockedTargetTracker()
    t.record(_tracked(LOCK_LOCKED), 0.0)
    assert t.effective_lock(0.0) is EffectiveLock.LOCKED
    assert t.effective_lock(0.0).should_command is True
    tgt = t.locked_target(0.0)
    assert tgt is not None and tgt.track_id == 7

    t.record(_tracked(LOCK_UNCERTAIN), 0.1)
    assert t.effective_lock(0.1) is EffectiveLock.UNCERTAIN
    assert t.effective_lock(0.1).should_command is False
    assert t.locked_target(0.1) is None  # hold, no command


def test_raw_lost_is_immediate() -> None:
    t = LockedTargetTracker()
    t.record(_tracked(LOCK_LOCKED), 0.0)
    t.record(_tracked(LOCK_LOST), 0.05)
    assert t.effective_lock(0.05) is EffectiveLock.LOST


def test_coast_window_holds_then_loses() -> None:
    t = LockedTargetTracker(coast_window_s=1.5)
    t.record(_tracked(LOCK_LOCKED), 10.0)
    # A batch with no tracked detection does not refresh the sighting.
    t.record(_Batch(detections=[_Det()]), 10.4)
    # Within the coast window it still reads locked (coasting on the last good).
    assert t.effective_lock(11.0) is EffectiveLock.LOCKED
    # Past the coast window it is lost.
    assert t.effective_lock(11.6) is EffectiveLock.LOST


def test_drop_requires_redesignate() -> None:
    t = LockedTargetTracker()
    t.record(_tracked(LOCK_LOCKED), 0.0)
    t.record(_tracked(LOCK_LOST), 0.1)
    assert t.effective_lock(0.1) is EffectiveLock.LOST
    t.drop()
    assert t.has_lock is False
    assert t.effective_lock(0.2) is EffectiveLock.NONE
    # A fresh operator designate (a subsequent tracked batch) re-locks.
    t.record(_tracked(LOCK_LOCKED, tid=9), 0.3)
    assert t.has_lock is True
    assert t.track_id == 9
    assert t.effective_lock(0.3) is EffectiveLock.LOCKED


def test_negative_coast_clamps_to_zero() -> None:
    # A negative window must not flip fail-safe (everything instantly lost).
    t = LockedTargetTracker(coast_window_s=-5.0)
    t.record(_tracked(LOCK_LOCKED), 0.0)
    assert t.effective_lock(0.0) is EffectiveLock.LOCKED
    assert t.effective_lock(0.001) is EffectiveLock.LOST


def test_default_coast_window() -> None:
    assert DEFAULT_COAST_WINDOW_S == 1.5
