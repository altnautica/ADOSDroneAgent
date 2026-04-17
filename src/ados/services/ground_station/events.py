"""Mesh and pairing event buses for the ground-station profile.

Mirrors the `ButtonEventBus` pattern in `ados.services.ui.events` but scoped
to distributed receive concerns: role transitions, batman-adv neighbor
churn, gateway election changes, and field pairing lifecycle.

Both buses are pure asyncio with per-subscriber queues so slow consumers
do not block the publisher. Used by:

- `role_manager` publishes role transitions.
- `mesh_manager` publishes neighbor join/leave, partition detected, and
  gateway election changes.
- `pairing_manager` publishes pair window open/close, request received,
  approval applied, and revocation applied.
- The REST `/api/v1/ground-station/mesh/events` WebSocket fans events out
  to GCS clients.
- OLED `screens/mesh/*` subscribes to refresh state without polling.

Consumers should treat the bus as "best-effort telemetry." Dropped events
are acceptable; authoritative state always lives in the managers.
"""

from __future__ import annotations

import asyncio
from dataclasses import dataclass, field
from typing import Any, AsyncIterator, Literal

__all__ = [
    "MeshEvent",
    "MeshEventBus",
    "PairingEvent",
    "PairingEventBus",
]


# Mesh event kinds:
# - role_changed: ground_station.role transitioned to a new value
# - neighbor_join: batman-adv saw a new peer on bat0
# - neighbor_leave: peer dropped past the dead-neighbor timeout
# - partition_detected: our node is in a partition missing known peers
# - partition_healed: mesh merged back with previously missing peers
# - gateway_changed: batctl gw_mode client picked a different gateway
# - relay_connected: receiver confirmed a relay is forwarding fragments
# - relay_disconnected: receiver stopped seeing fragments from a relay
# - receiver_unreachable: relay lost its receiver (mDNS timeout)
_MESH_EVENT_KINDS = Literal[
    "role_changed",
    "neighbor_join",
    "neighbor_leave",
    "partition_detected",
    "partition_healed",
    "gateway_changed",
    "relay_connected",
    "relay_disconnected",
    "receiver_unreachable",
]


@dataclass(frozen=True)
class MeshEvent:
    kind: _MESH_EVENT_KINDS
    timestamp_ms: int
    payload: dict[str, Any] = field(default_factory=dict)


_PAIRING_EVENT_KINDS = Literal[
    "accept_window_opened",
    "accept_window_closed",
    "join_request_received",
    "join_approved",
    "join_rejected",
    "join_completed",
    "revoked",
    "psk_mismatch",
    "bundle_expired",
]


@dataclass(frozen=True)
class PairingEvent:
    kind: _PAIRING_EVENT_KINDS
    timestamp_ms: int
    payload: dict[str, Any] = field(default_factory=dict)


class _FanoutBus:
    """Shared fanout implementation for MeshEventBus and PairingEventBus."""

    _SENTINEL: object = object()

    def __init__(self, queue_maxsize: int = 128) -> None:
        self._subscribers: list[asyncio.Queue] = []
        self._queue_maxsize = queue_maxsize
        self._closed = False
        self._lock = asyncio.Lock()

    async def _publish(self, event: Any) -> None:
        if self._closed:
            return
        async with self._lock:
            targets = list(self._subscribers)
        for q in targets:
            try:
                q.put_nowait(event)
            except asyncio.QueueFull:
                # Drop for slow subscriber; bus stays live for others.
                pass

    async def _subscribe(self) -> AsyncIterator[Any]:
        queue: asyncio.Queue = asyncio.Queue(maxsize=self._queue_maxsize)
        async with self._lock:
            if self._closed:
                return
            self._subscribers.append(queue)
        try:
            while True:
                item = await queue.get()
                if item is self._SENTINEL:
                    return
                yield item
        finally:
            async with self._lock:
                if queue in self._subscribers:
                    self._subscribers.remove(queue)

    async def close(self) -> None:
        async with self._lock:
            self._closed = True
            targets = list(self._subscribers)
        for q in targets:
            try:
                q.put_nowait(self._SENTINEL)
            except asyncio.QueueFull:
                try:
                    q.get_nowait()
                    q.put_nowait(self._SENTINEL)
                except Exception:
                    pass


class MeshEventBus(_FanoutBus):
    async def publish(self, event: MeshEvent) -> None:
        await self._publish(event)

    async def subscribe(self) -> AsyncIterator[MeshEvent]:
        async for item in self._subscribe():
            assert isinstance(item, MeshEvent)
            yield item


class PairingEventBus(_FanoutBus):
    async def publish(self, event: PairingEvent) -> None:
        await self._publish(event)

    async def subscribe(self) -> AsyncIterator[PairingEvent]:
        async for item in self._subscribe():
            assert isinstance(item, PairingEvent)
            yield item


# Process-local singletons. The API router, mesh_manager, role_manager,
# pairing_manager, and OLED screens all import these. Tests can replace
# the module attributes directly.
_mesh_bus: MeshEventBus | None = None
_pairing_bus: PairingEventBus | None = None


def get_mesh_event_bus() -> MeshEventBus:
    global _mesh_bus
    if _mesh_bus is None:
        _mesh_bus = MeshEventBus()
    return _mesh_bus


def get_pairing_event_bus() -> PairingEventBus:
    global _pairing_bus
    if _pairing_bus is None:
        _pairing_bus = PairingEventBus()
    return _pairing_bus
