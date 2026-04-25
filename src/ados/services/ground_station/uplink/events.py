"""Uplink event types and fanout bus.

`UplinkEvent` is the canonical record for routing, health, and data-cap
state changes. `UplinkEventBus` mirrors `ButtonEventBus` from
`ados.services.ui.events` and `PicEventBus` from `pic_arbiter`: bounded
per-subscriber queues, drop-on-full, structural fanout.
"""

from __future__ import annotations

import asyncio
from dataclasses import dataclass
from typing import AsyncIterator, Literal, Optional

__all__ = [
    "UplinkEventKind",
    "DataCapState",
    "UplinkEvent",
    "UplinkEventBus",
]


UplinkEventKind = Literal["uplink_changed", "health_changed", "data_cap_threshold"]
DataCapState = Literal["ok", "warn_80", "throttle_95", "blocked_100"]


@dataclass(frozen=True)
class UplinkEvent:
    """A routing, health, or data-cap state change."""

    kind: UplinkEventKind
    active_uplink: Optional[str]
    available: list[str]
    internet_reachable: bool
    data_cap_state: Optional[DataCapState]
    timestamp_ms: int


class UplinkEventBus:
    """Fanout bus for `UplinkEvent`. Bounded queues, drop-on-full."""

    _SENTINEL: object = object()

    def __init__(self, queue_maxsize: int = 64) -> None:
        self._subscribers: list[asyncio.Queue] = []
        self._queue_maxsize = queue_maxsize
        self._closed = False
        self._lock = asyncio.Lock()

    async def publish(self, event: UplinkEvent) -> None:
        if self._closed:
            return
        async with self._lock:
            targets = list(self._subscribers)
        for q in targets:
            try:
                q.put_nowait(event)
            except asyncio.QueueFull:
                pass

    async def subscribe(self) -> AsyncIterator[UplinkEvent]:
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
                assert isinstance(item, UplinkEvent)
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
