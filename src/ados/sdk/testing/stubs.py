"""In-process IPC stub backing :class:`PluginTestHarness`.

The fake client matches the public surface :class:`PluginContext`
consumes (``event_publish``, ``event_subscribe``, ``ping``, ``close``)
without spinning up a UDS or a supervisor. Capability checks fire the
same :class:`CapabilityDenied` exception the real client raises so
plugin code under test sees the production failure mode.
"""

from __future__ import annotations

import asyncio
import fnmatch
from collections.abc import Awaitable, Callable
from typing import Any

from ados.plugins.errors import CapabilityDenied

EventCallback = Callable[[dict], Awaitable[None] | None]


class FakeIpcClient:
    """Duck-typed stand-in for :class:`PluginIpcClient`.

    Methods mirror the subset :class:`PluginContext` reaches into. The
    harness owns one instance; plugin code consumes it indirectly via
    the context's ``events`` and ``ping_supervisor`` accessors.
    """

    def __init__(self, *, plugin_id: str, granted_capabilities: set[str]) -> None:
        self.plugin_id = plugin_id
        self._granted = set(granted_capabilities)
        self._subs: dict[str, list[EventCallback]] = {}
        self.published: list[tuple[str, dict[str, Any]]] = []

    def grant(self, capability: str) -> None:
        self._granted.add(capability)

    def revoke(self, capability: str) -> None:
        self._granted.discard(capability)

    @property
    def granted_capabilities(self) -> frozenset[str]:
        return frozenset(self._granted)

    async def event_publish(self, topic: str, payload: dict[str, Any]) -> int:
        if "event.publish" not in self._granted:
            raise CapabilityDenied(self.plugin_id, "event.publish")
        self.published.append((topic, dict(payload)))
        return await self._deliver(topic, payload)

    async def event_subscribe(
        self, topic_pattern: str, callback: EventCallback
    ) -> None:
        if "event.subscribe" not in self._granted:
            raise CapabilityDenied(self.plugin_id, "event.subscribe")
        self._subs.setdefault(topic_pattern, []).append(callback)

    async def ping(self) -> dict[str, Any]:
        return {"pong": True, "plugin_id": self.plugin_id}

    async def close(self) -> None:
        return None

    async def deliver(self, topic: str, payload: dict[str, Any]) -> int:
        """Inject an event from the harness as if a peer had published it.

        Bypasses the publish capability check; the harness models
        external sources whose capability the plugin under test does
        not own.
        """
        return await self._deliver(topic, payload)

    async def _deliver(self, topic: str, payload: dict[str, Any]) -> int:
        delivered = 0
        for pattern, callbacks in list(self._subs.items()):
            if pattern == topic or fnmatch.fnmatchcase(topic, pattern):
                for cb in callbacks:
                    result = cb(payload if isinstance(payload, dict) else {})
                    if asyncio.iscoroutine(result):
                        await result
                    delivered += 1
        return delivered
