"""Button event bus.

Defines the `ButtonEvent` dataclass and a fanout `ButtonEventBus` that
lets multiple consumers (OLED service, logging, tests) each receive
every published event on their own queue.

The bus is pure asyncio. It has no GPIO dependency, so tests and the
OLED service can subscribe without hardware. The button service in
`button_service.py` is the normal publisher but any code path can
publish synthetic events for simulation or integration tests.

Contract:

    bus = ButtonEventBus()
    async for event in bus.subscribe():
        handle(event)

    await bus.publish(ButtonEvent(button=5, kind="short", timestamp_ms=...))
    await bus.close()  # unblocks all subscribers with StopAsyncIteration
"""

from __future__ import annotations

import asyncio
from dataclasses import dataclass
from typing import AsyncIterator, Literal

__all__ = ["ButtonEvent", "ButtonEventBus"]


@dataclass(frozen=True)
class ButtonEvent:
    """A single button press observation.

    button: GPIO pin number (BCM numbering).
    kind: "short" or "long". Long press threshold is decided by the
        publisher (button service uses 2 seconds).
    timestamp_ms: monotonic milliseconds captured at the edge that
        finalized the event. For short press this is the release edge.
        For long press this is also the release edge.
    """

    button: int
    kind: Literal["short", "long"]
    timestamp_ms: int
    # Resolved action name from the live button mapping in
    # `ground_station.ui.buttons.mapping`. None when the mapping has
    # no entry for this `(button, kind)` pair, or when the publisher
    # has no mapping context (e.g. tests, simulation). Consumers may
    # treat None as "use default for this button".
    action: str | None = None


class ButtonEventBus:
    """Asyncio fanout bus for ButtonEvent.

    Each subscriber gets its own `asyncio.Queue`. Publish copies the
    event into every subscriber queue. Slow subscribers do not block
    the publisher beyond a single queue.put_nowait attempt; if a
    subscriber queue is full the event is dropped for that subscriber
    and a warning is logged. This keeps the button service responsive
    even if a consumer stalls.
    """

    _SENTINEL: object = object()

    def __init__(self, queue_maxsize: int = 64) -> None:
        self._subscribers: list[asyncio.Queue] = []
        self._queue_maxsize = queue_maxsize
        self._closed = False
        self._lock = asyncio.Lock()

    async def publish(self, event: ButtonEvent) -> None:
        """Fan out one event to all current subscribers."""
        if self._closed:
            return
        async with self._lock:
            targets = list(self._subscribers)
        for q in targets:
            try:
                q.put_nowait(event)
            except asyncio.QueueFull:
                # Drop on the floor for this subscriber. The bus stays
                # live for other consumers.
                pass

    async def subscribe(self) -> AsyncIterator[ButtonEvent]:
        """Yield every event published from this point forward.

        Exits cleanly when `close()` is called.
        """
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
                assert isinstance(item, ButtonEvent)
                yield item
        finally:
            async with self._lock:
                if queue in self._subscribers:
                    self._subscribers.remove(queue)

    async def close(self) -> None:
        """Signal every subscriber to exit its iteration."""
        async with self._lock:
            self._closed = True
            targets = list(self._subscribers)
        for q in targets:
            try:
                q.put_nowait(self._SENTINEL)
            except asyncio.QueueFull:
                # Force-drain one item to make space for the sentinel.
                try:
                    q.get_nowait()
                    q.put_nowait(self._SENTINEL)
                except Exception:
                    pass
