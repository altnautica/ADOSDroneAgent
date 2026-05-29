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
from dataclasses import dataclass, field
from typing import Any

from ados.plugins.errors import CapabilityDenied

EventCallback = Callable[[dict], Awaitable[None] | None]


@dataclass
class _FakeEnvelope:
    """The shape :meth:`FakeIpcClient._send_request` returns: a response
    envelope with an ``args`` mapping, mirroring the real client's envelope."""

    args: dict[str, Any] = field(default_factory=dict)


class FakeIpcClient:
    """Duck-typed stand-in for :class:`PluginIpcClient`.

    Methods mirror the subset :class:`PluginContext` reaches into. The
    harness owns one instance; plugin code consumes it indirectly via
    the context's ``events``, ``vision``, and ``ping_supervisor``
    accessors.

    ``_send_request`` is the generic RPC entry the ``VisionClient`` uses,
    matching the real client's private sender. It enforces capabilities
    the same way, records every request as ``(method, args)``, and returns
    a canned response (seed one with :meth:`set_response`).
    """

    def __init__(self, *, plugin_id: str, granted_capabilities: set[str]) -> None:
        self.plugin_id = plugin_id
        self._granted = set(granted_capabilities)
        self._subs: dict[str, list[EventCallback]] = {}
        self.published: list[tuple[str, dict[str, Any]]] = []
        # Vision (and any other generic-RPC) bookkeeping.
        self.requests: list[tuple[str, dict[str, Any]]] = []
        self._responses: dict[str, dict[str, Any]] = {}
        self.registered_components: list[tuple[int, str]] = []
        self.sent_mavlink: list[tuple[bytes, int | None]] = []

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

    # ------------------------------------------------------------------
    # Generic RPC + MAVLink surface (used by the vision client).
    # ------------------------------------------------------------------

    def set_response(self, method: str, args: dict[str, Any]) -> None:
        """Seed the response a future ``_send_request`` for ``method``
        returns. Lets a test stage an ``infer`` result, for example."""
        self._responses[method] = dict(args)

    async def _send_request(
        self, method: str, *, capability: str, args: dict[str, Any]
    ) -> _FakeEnvelope:
        if capability and capability not in self._granted:
            raise CapabilityDenied(self.plugin_id, capability)
        self.requests.append((method, dict(args)))
        return _FakeEnvelope(args=dict(self._responses.get(method, {})))

    async def mavlink_send(
        self, msg_bytes: bytes, component_id: int | None = None
    ) -> dict[str, Any]:
        if "mavlink.write" not in self._granted:
            raise CapabilityDenied(self.plugin_id, "mavlink.write")
        self.sent_mavlink.append((bytes(msg_bytes), component_id))
        return {"sent": True}

    async def mavlink_register_component(
        self, comp_id: int, kind: str
    ) -> dict[str, Any]:
        cap = f"mavlink.component.{kind}"
        if cap not in self._granted:
            raise CapabilityDenied(self.plugin_id, cap)
        self.registered_components.append((int(comp_id), kind))
        return {"registered": True}
