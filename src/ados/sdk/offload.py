"""Perception-offload safety primitives for plugins (the Follow-Me gate).

A behaviour that consumes offloaded detections / poses (a target relayed back
from the compute node) runs its fast loop locally and gates the slow remote
layer with these primitives — the plugin-side mirror of the agent's
``ados-offload`` logic. The one invariant: a result past its freshness budget,
or a dropped link, is treated as absent — the behaviour stops and holds, never
extrapolates, and never auto-re-acquires a dropped lock (only an explicit
re-designate re-locks).

This is the link-aware tightening of the shipped Follow-Me lock-state gate,
usable by any offloaded-vision behaviour.
"""

from __future__ import annotations

import enum
from dataclasses import dataclass


class ExecutionTier(str, enum.Enum):
    """Where a plugin's inference model runs, chosen when it opens a stream.

    ``LOCAL`` runs the model on the drone's own accelerator (the existing vision
    path — the plugin registers its model with the engine). ``OFFLOAD`` runs it
    on a paired compute node: the drone streams its camera to the node, the node
    runs the model, and detections return onto the drone's shared
    ``vision.detection`` bus. ``AUTO`` lets the runtime pick — local when the
    drone has a usable accelerator, offload when it is NPU-less and a compute
    node is paired. Either way the detections land on the same bus, so downstream
    (the cockpit, other plugins, the lock/behaviour gate) is
    execution-transparent.

    The tier decision itself is the agent's, from ``ados_offload::pick_tier``
    reading the offload-link sidecar; it is NOT reimplemented here. A plugin
    passes its intent to ``ctx.compute.open_stream`` and the host resolves the
    tier and reports it back on the session (``session.execution``).
    """

    LOCAL = "local"
    """Run the model on the drone's own accelerator (the local vision path)."""
    OFFLOAD = "offload"
    """Run the model on the paired compute node; detections stream back."""
    AUTO = "auto"
    """Let the runtime pick local vs offload from the live perception tier."""


class GateState(str, enum.Enum):
    """The freshness of one offloaded stream."""

    FRESH = "fresh"  # within budget + link up -> usable
    STALE = "stale"  # a result, but older than the budget -> stop
    LOST = "lost"  # no result yet, or the link is down -> stop

    @property
    def is_usable(self) -> bool:
        return self is GateState.FRESH


@dataclass
class FreshnessGate:
    """Tracks one offloaded stream's freshness.

    Anchored on the LOCAL MONOTONIC time the result arrived, not the result's
    own (remote) timestamp: the drone and the compute node are not clock-synced,
    so a node whose clock is skewed ahead and whose detector then hangs (socket
    up, so no link drop) would keep a frozen stream reading fresh forever if age
    came from the remote timestamp. Measuring "time since a result last arrived,
    on the drone's own monotonic clock" catches that stall. The ``now_ms`` passed
    to ``record`` and ``state`` MUST be the same local monotonic clock (e.g.
    ``time.monotonic_ns() // 1_000_000``); the remote timestamp is for telemetry,
    never for this gate.
    """

    budget_ms: int
    _last_arrival_ms: int | None = None
    _link_up: bool = True

    def __post_init__(self) -> None:
        # A negative budget must not flip the fail-safe direction (it would make
        # everything stale and silently disable a behaviour); clamp to 0.
        if self.budget_ms < 0:
            self.budget_ms = 0

    def record(self, now_ms: int) -> None:
        """A result arrived. ``now_ms`` is the LOCAL MONOTONIC time of arrival,
        not the result's own timestamp."""
        self._last_arrival_ms = now_ms

    def set_link(self, up: bool) -> None:
        self._link_up = up

    def state(self, now_ms: int) -> GateState:
        if not self._link_up:
            return GateState.LOST
        if self._last_arrival_ms is None:
            return GateState.LOST
        age = now_ms - self._last_arrival_ms
        return GateState.FRESH if age <= self.budget_ms else GateState.STALE

    def is_usable(self, now_ms: int) -> bool:
        return self.state(now_ms).is_usable


class LockState(str, enum.Enum):
    UNLOCKED = "unlocked"
    LOCKED = "locked"  # acquiring or tracking
    LOST = "lost"  # had a lock, lost it; never auto-re-acquires


class LockGate:
    """The lock-state safety gate.

    Locked is *acquiring* until the stream is first usable; once it has tracked,
    the stream going stale or the link dropping drops it to ``LOST``. The only
    way out of ``LOST`` is :meth:`lock` (an explicit re-designate). A lock that
    has never had a usable reading stays ``LOCKED`` (holding), never ``LOST``.
    """

    def __init__(self) -> None:
        self._state = LockState.UNLOCKED
        self._ever_usable_since_lock = False

    @property
    def state(self) -> LockState:
        return self._state

    @property
    def is_locked(self) -> bool:
        return self._state is LockState.LOCKED

    def lock(self) -> None:
        """Acquire / re-acquire — the ONLY transition out of LOST."""
        self._state = LockState.LOCKED
        self._ever_usable_since_lock = False

    def unlock(self) -> None:
        self._state = LockState.UNLOCKED
        self._ever_usable_since_lock = False

    def update(self, stream_usable: bool) -> None:
        if self._state is not LockState.LOCKED:
            return
        if stream_usable:
            self._ever_usable_since_lock = True
        elif self._ever_usable_since_lock:
            self._state = LockState.LOST
