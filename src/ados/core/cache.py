"""TTL cache for hot-path values. In-memory, single-process, async-safe.

Used by the API layer to memoize expensive computations (board detect,
external binary lookups, profile YAML reads, mesh state reads, service
list snapshots) across high-rate poll endpoints. The cache is process
local and does not persist across restarts.

Concurrent callers that miss on the same key wait on a per-key lock so
the underlying fetch runs once. Stale values are silently overwritten
when the TTL expires.
"""

from __future__ import annotations

import asyncio
import time
from dataclasses import dataclass
from typing import Any, Awaitable, Callable, Generic, TypeVar

T = TypeVar("T")


@dataclass
class _Entry(Generic[T]):
    value: T
    expires_at: float


class TTLCache:
    """Async-safe in-memory TTL cache keyed by string.

    The cache is intentionally minimal: no max size, no LRU eviction,
    no metrics. Hot-path callers should use stable, low-cardinality
    keys (a handful of well-known names) so unbounded growth is not
    a concern.
    """

    def __init__(self) -> None:
        self._entries: dict[str, _Entry[Any]] = {}
        self._locks: dict[str, asyncio.Lock] = {}
        self._global_lock = asyncio.Lock()

    async def get(
        self,
        key: str,
        fetch: Callable[[], Awaitable[T]],
        ttl_seconds: float,
    ) -> T:
        """Return the cached value for `key`, refreshing via `fetch` if expired.

        `fetch` must be an async callable taking no arguments. Sync work
        should be wrapped via `asyncio.to_thread(...)` inside the fetch.
        """
        now = time.monotonic()
        entry = self._entries.get(key)
        if entry is not None and entry.expires_at > now:
            return entry.value

        async with self._global_lock:
            lock = self._locks.setdefault(key, asyncio.Lock())

        async with lock:
            entry = self._entries.get(key)
            now = time.monotonic()
            if entry is not None and entry.expires_at > now:
                return entry.value
            value = await fetch()
            self._entries[key] = _Entry(value=value, expires_at=now + ttl_seconds)
            return value

    def invalidate(self, key: str) -> None:
        """Drop the entry for `key`. Safe to call even if absent."""
        self._entries.pop(key, None)

    def invalidate_all(self) -> None:
        """Clear every entry. Locks are kept (cheap to reuse)."""
        self._entries.clear()


# Module-level shared cache for the consolidated status endpoint and
# adjacent hot-path routes. Callers should agree on stable key names.
status_cache = TTLCache()
