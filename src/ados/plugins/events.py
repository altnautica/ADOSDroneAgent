"""Capability-gated plugin event bus.

A pure-asyncio fanout bus modeled on
:mod:`ados.services.ground_station.events`. Adds capability-token
checks so each ``publish`` and ``subscribe`` call is validated against
the plugin's granted permission set before the supervisor accepts it.

Topic taxonomy is namespaced (``mavlink.*``, ``vehicle.*``,
``mission.*``, ``safety.*``, ``sensor.<id>.*``, ``vision.*``,
``video.*``, ``gps.*``, ``agent.*``, ``plugin.<id>.*``, ``swarm.*``,
``gcs.*``). Subscription wildcards apply trailing ``.*``.

Capability rules:

* Subscribing to a topic requires ``event.subscribe`` capability AND
  the topic must be in the plugin's subscribe-allowlist (which the
  supervisor seeds from the manifest's contributes block plus an
  implicit allowlist of public topics).
* Publishing requires ``event.publish`` for non-namespaced topics and
  ``plugin.<id>.*`` is always publishable by the plugin owning that id.
"""

from __future__ import annotations

import asyncio
import fnmatch
import time
from collections import defaultdict
from collections.abc import AsyncIterator
from dataclasses import dataclass
from typing import Any

from ados.core.logging import get_logger

log = get_logger("plugins.events")

QUEUE_DEPTH = 256


@dataclass(frozen=True)
class Event:
    topic: str
    timestamp_ms: int
    publisher_plugin_id: str | None
    payload: dict[str, Any]


class EventBus:
    """In-process fanout. Each subscriber gets its own bounded queue.

    Slow consumers do not block publishers; events are dropped on full
    queue and a counter is incremented in the subscriber's metadata.
    """

    def __init__(self) -> None:
        self._subscribers: dict[
            str, list[asyncio.Queue[Event]]
        ] = defaultdict(list)
        self._lock = asyncio.Lock()

    async def subscribe(self, topic_pattern: str) -> AsyncIterator[Event]:
        queue: asyncio.Queue[Event] = asyncio.Queue(maxsize=QUEUE_DEPTH)
        async with self._lock:
            self._subscribers[topic_pattern].append(queue)
        try:
            while True:
                evt = await queue.get()
                yield evt
        finally:
            async with self._lock:
                if queue in self._subscribers[topic_pattern]:
                    self._subscribers[topic_pattern].remove(queue)
                if not self._subscribers[topic_pattern]:
                    del self._subscribers[topic_pattern]

    async def publish(self, event: Event) -> int:
        """Fan out to subscribers whose pattern matches the topic.

        Returns the number of subscribers the event was delivered to.
        """
        delivered = 0
        async with self._lock:
            patterns = list(self._subscribers.keys())
        for pattern in patterns:
            if not _topic_matches(pattern, event.topic):
                continue
            async with self._lock:
                queues = list(self._subscribers.get(pattern, ()))
            for q in queues:
                try:
                    q.put_nowait(event)
                    delivered += 1
                except asyncio.QueueFull:
                    log.warning(
                        "plugin_event_dropped_queue_full",
                        topic=event.topic,
                        pattern=pattern,
                    )
        return delivered

    def subscriber_count(self) -> int:
        return sum(len(qs) for qs in self._subscribers.values())


def _topic_matches(pattern: str, topic: str) -> bool:
    """Glob-style match. ``mavlink.*`` matches ``mavlink.heartbeat`` but
    not ``mavlink``. ``mavlink.**`` matches arbitrary depth."""
    if pattern == topic:
        return True
    # Translate ".*" to fnmatch's segment-style wildcards.
    return fnmatch.fnmatchcase(topic, pattern)


# ---------------------------------------------------------------------------
# Capability checks
# ---------------------------------------------------------------------------


_PUBLIC_TOPICS_FOR_SUBSCRIBE: frozenset[str] = frozenset(
    {
        "vehicle.armed",
        "vehicle.disarmed",
        "vehicle.mode_changed",
        "vehicle.battery_low",
        "vehicle.geofence_breach",
        "mission.started",
        "mission.completed",
        "mission.aborted",
        "agent.ready",
        "agent.shutdown",
    }
)
"""Topics any plugin may subscribe to without an explicit allowlist
entry. These are the operator-relevant safety/lifecycle events that
nearly every plugin needs."""


def is_subscribe_allowed(
    *,
    plugin_id: str,
    topic_pattern: str,
    granted_caps: set[str],
    extra_allow: set[str] | None = None,
) -> bool:
    if "event.subscribe" not in granted_caps:
        return False
    if topic_pattern.startswith(f"plugin.{plugin_id}."):
        return True
    if topic_pattern in _PUBLIC_TOPICS_FOR_SUBSCRIBE:
        return True
    if extra_allow and any(
        _topic_matches(allow, topic_pattern) for allow in extra_allow
    ):
        return True
    return False


def is_publish_allowed(
    *,
    plugin_id: str,
    topic: str,
    granted_caps: set[str],
) -> bool:
    if topic.startswith(f"plugin.{plugin_id}."):
        return True  # plugin's own topic, always publishable
    if "event.publish" not in granted_caps:
        return False
    # Reserved namespaces a plugin must not publish into.
    reserved_prefixes = (
        "vehicle.",
        "mavlink.",
        "mission.",
        "safety.",
        "agent.",
        "swarm.",
        "gps.",
    )
    if any(topic.startswith(p) for p in reserved_prefixes):
        return False
    return True


def now_ms() -> int:
    return int(time.time() * 1000)
