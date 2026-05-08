"""Touch event dataclasses + fanout buses.

Two buses run in parallel:

* :class:`TouchMoveBus` — every meaningful movement sample while the
  pen is down. Pages that draw a live drag indicator subscribe here.
* :class:`TouchEventBus` — completed gestures emitted on pen-up. Pages
  that react to taps, swipes, and long presses subscribe here.

Both follow the same pattern as the existing :class:`ButtonEventBus`:
each subscriber gets an asyncio.Queue, slow consumers drop on the
floor instead of stalling the publisher, ``close()`` unblocks every
subscriber with a sentinel.
"""

from __future__ import annotations

import asyncio
from collections.abc import AsyncIterator
from dataclasses import dataclass
from typing import Literal

GestureKind = Literal["tap", "long_press", "swipe", "drag"]
Direction = Literal["up", "down", "left", "right"]


@dataclass(frozen=True)
class TouchMove:
    """A single live position sample while the pen is down.

    Coordinates are in LCD pixel space (after rotation + calibration
    have been applied). ``timestamp_ms`` is monotonic milliseconds.
    """

    x_lcd: int
    y_lcd: int
    timestamp_ms: int


@dataclass(frozen=True)
class TouchGesture:
    """A completed pen-down -> pen-up sequence classified into a kind.

    * ``tap`` — short contact, no significant motion.
    * ``long_press`` — long contact at roughly the same point.
    * ``swipe`` — short contact with significant motion in one
      cardinal direction.
    * ``drag`` — long contact with motion (pages may scroll).

    ``direction`` is set for swipes and drags; tap/long_press leave it
    None. ``velocity_px_per_s`` is the average velocity over the whole
    gesture in LCD pixels per second; it is what kinetic decay uses
    to seed momentum on a drag-release.

    ``samples`` is the full sequence of (x_lcd, y_lcd, timestamp_ms)
    captured during the contact, useful for pages that want to draw a
    trail or replay the path.
    """

    kind: GestureKind
    start_x: int
    start_y: int
    end_x: int
    end_y: int
    start_t_ms: int
    end_t_ms: int
    duration_ms: int
    direction: Direction | None
    velocity_px_per_s: float
    samples: tuple[tuple[int, int, int], ...]


_SENTINEL: object = object()


class _BaseBus:
    """Shared fanout machinery for the move and gesture buses."""

    def __init__(self, queue_maxsize: int = 64) -> None:
        self._subscribers: list[asyncio.Queue] = []
        self._queue_maxsize = queue_maxsize
        self._closed = False
        self._lock = asyncio.Lock()

    async def _publish(self, item: object) -> None:
        if self._closed:
            return
        async with self._lock:
            targets = list(self._subscribers)
        for q in targets:
            try:
                q.put_nowait(item)
            except asyncio.QueueFull:
                # Slow subscriber. Drop on the floor — the publisher
                # must stay non-blocking so a stalled consumer cannot
                # back-pressure the touch reader.
                pass

    async def _subscribe_iter(self) -> AsyncIterator[object]:
        queue: asyncio.Queue = asyncio.Queue(maxsize=self._queue_maxsize)
        async with self._lock:
            if self._closed:
                return
            self._subscribers.append(queue)
        try:
            while True:
                item = await queue.get()
                if item is _SENTINEL:
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
                q.put_nowait(_SENTINEL)
            except asyncio.QueueFull:
                try:
                    q.get_nowait()
                    q.put_nowait(_SENTINEL)
                except Exception:
                    pass


class TouchMoveBus(_BaseBus):
    """Live touch-move samples. Subscribers receive every movement."""

    async def publish(self, move: TouchMove) -> None:
        await self._publish(move)

    async def subscribe(self) -> AsyncIterator[TouchMove]:
        async for item in self._subscribe_iter():
            assert isinstance(item, TouchMove)
            yield item


class TouchEventBus(_BaseBus):
    """Completed gesture bus. Subscribers receive one event per pen-up."""

    async def publish(self, gesture: TouchGesture) -> None:
        await self._publish(gesture)

    async def subscribe(self) -> AsyncIterator[TouchGesture]:
        async for item in self._subscribe_iter():
            assert isinstance(item, TouchGesture)
            yield item
