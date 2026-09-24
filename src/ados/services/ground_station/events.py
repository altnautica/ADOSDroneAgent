"""Ground-station event shapes: the in-process pairing bus.

The GCS mesh-events WebSocket is served natively. It tails two newline-JSON
journals under ``/run/ados``: ``mesh-events.jsonl`` (written by the native
groundlink data plane and the supervisor's role transitions) and
``pair-events.jsonl`` (mirrored by :mod:`pair_journal`). An in-process bus
reaches only subscribers in the same process, so mesh events are journaled,
never published on a bus.
"""

from __future__ import annotations

import asyncio
from collections.abc import AsyncIterator
from dataclasses import dataclass, field
from typing import Any, Literal

__all__ = [
    "PairingEvent",
    "PairingEventBus",
]


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
    """Bounded per-subscriber fanout: a slow consumer never blocks the publisher."""

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


class PairingEventBus(_FanoutBus):
    async def publish(self, event: PairingEvent) -> None:
        await self._publish(event)

    async def subscribe(self) -> AsyncIterator[PairingEvent]:
        async for item in self._subscribe():
            assert isinstance(item, PairingEvent)
            yield item


# Process-local singleton. Tests can replace the module attribute directly.
_pairing_bus: PairingEventBus | None = None


def get_pairing_event_bus() -> PairingEventBus:
    global _pairing_bus
    if _pairing_bus is None:
        _pairing_bus = PairingEventBus()
    return _pairing_bus
