"""Path-keyed observable state for the LCD page system.

Pages (and the chrome) often want to redraw only when a small slice
of agent state changes — RSSI moved, the role flipped, the pair code
expired. Subscribing to the entire state dict is too coarse and blows
through the framebuffer's redraw budget on busy boards.

:class:`StateBus` solves this with dotted-path keys::

    bus.set("link.rssi_dbm", -54)         # publishes only on change
    bus.subscribe("link.rssi_dbm", on_rssi)

Callbacks fire only when ``set`` writes a value that differs from
what's already at that path. Subscribers can be sync or async (a
coroutine result is awaited by the caller of ``set`` if used inside
an async context). Non-existent paths return ``None`` from
``get(path)``.

This module exists in C2 as the foundation pages will subscribe to
in C3+. Today it has the read/write/subscribe surface fully wired so
tests can exercise it end-to-end.
"""

from __future__ import annotations

import asyncio
import threading
from collections.abc import Callable
from typing import Any

PathCallback = Callable[[Any, Any], Any]
"""``(old_value, new_value) -> Any``. Awaited if the result is a coroutine."""


class StateBus:
    """Observable nested-dict state with dotted-path subscribe."""

    def __init__(self) -> None:
        self._values: dict[str, Any] = {}
        self._subscribers: dict[str, list[PathCallback]] = {}
        # threading.Lock is fine here — the page navigator runs in one
        # event loop thread, but the agent's REST handlers may write
        # state from worker threads. The lock is taken only for the
        # short duration of the dict mutation.
        self._lock = threading.Lock()

    def set(self, path: str, value: Any) -> bool:
        """Write ``value`` at ``path`` and notify subscribers on change.

        Returns True if the value differed from the previous one (and
        callbacks were dispatched), False if it was already up to date.
        """
        with self._lock:
            old = self._values.get(path, _MISSING)
            if old == value:
                return False
            self._values[path] = value
            callbacks = list(self._subscribers.get(path, ()))
        for cb in callbacks:
            self._dispatch(cb, old if old is not _MISSING else None, value)
        return True

    def get(self, path: str, default: Any = None) -> Any:
        with self._lock:
            return self._values.get(path, default)

    def subscribe(self, path: str, callback: PathCallback) -> Callable[[], None]:
        """Register ``callback`` for changes at ``path``.

        Returns an unsubscribe function. The callback is invoked
        synchronously by ``set``; if it returns a coroutine the
        coroutine is scheduled on the running event loop (when one
        is available) or run to completion via ``asyncio.run``.
        """
        with self._lock:
            self._subscribers.setdefault(path, []).append(callback)

        def _unsubscribe() -> None:
            with self._lock:
                bucket = self._subscribers.get(path)
                if not bucket:
                    return
                try:
                    bucket.remove(callback)
                except ValueError:
                    return
                if not bucket:
                    self._subscribers.pop(path, None)

        return _unsubscribe

    def snapshot(self) -> dict[str, Any]:
        """Return a shallow copy of every path -> value entry."""
        with self._lock:
            return dict(self._values)

    # ── private helpers ─────────────────────────────────────────

    def _dispatch(
        self,
        cb: PathCallback,
        old: Any,
        new: Any,
    ) -> None:
        try:
            result = cb(old, new)
        except Exception:
            # Subscribers must not break the publisher; swallow + move
            # on. The bus is best-effort by design.
            return
        if asyncio.iscoroutine(result):
            try:
                loop = asyncio.get_running_loop()
            except RuntimeError:
                # No loop on this thread; run the coroutine to
                # completion. Pages live on the event loop thread so
                # this branch is reserved for tests.
                try:
                    asyncio.run(result)
                except Exception:
                    pass
                return
            try:
                loop.create_task(result)
            except RuntimeError:
                pass


_MISSING = object()
