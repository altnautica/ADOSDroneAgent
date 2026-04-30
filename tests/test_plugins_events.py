"""Plugin event bus tests.

Covers the in-process fanout, capability gates, public-topic
allowlist, plugin-namespace publish carve-out, and slow-consumer
queue-full drop behavior.
"""

from __future__ import annotations

import asyncio

import pytest

from ados.plugins.events import (
    Event,
    EventBus,
    is_publish_allowed,
    is_subscribe_allowed,
    now_ms,
)


# ---------------------------------------------------------------------
# EventBus fanout
# ---------------------------------------------------------------------


@pytest.mark.asyncio
async def test_publish_with_no_subscribers_returns_zero() -> None:
    bus = EventBus()
    delivered = await bus.publish(
        Event(topic="vehicle.armed", timestamp_ms=now_ms(),
              publisher_plugin_id=None, payload={})
    )
    assert delivered == 0


@pytest.mark.asyncio
async def test_subscribe_exact_topic_receives_event() -> None:
    bus = EventBus()

    async def reader(out: list[Event]) -> None:
        async for evt in bus.subscribe("vehicle.armed"):
            out.append(evt)
            return

    got: list[Event] = []
    task = asyncio.create_task(reader(got))
    await asyncio.sleep(0)  # let reader register
    delivered = await bus.publish(
        Event(topic="vehicle.armed", timestamp_ms=now_ms(),
              publisher_plugin_id="com.example.x", payload={"k": 1})
    )
    await asyncio.wait_for(task, timeout=1.0)
    assert delivered == 1
    assert got[0].topic == "vehicle.armed"
    assert got[0].payload == {"k": 1}


@pytest.mark.asyncio
async def test_wildcard_pattern_matches_subtopics() -> None:
    bus = EventBus()

    async def reader(out: list[Event]) -> None:
        async for evt in bus.subscribe("plugin.com.example.x.*"):
            out.append(evt)
            if len(out) >= 2:
                return

    got: list[Event] = []
    task = asyncio.create_task(reader(got))
    await asyncio.sleep(0)
    await bus.publish(
        Event(topic="plugin.com.example.x.alert", timestamp_ms=now_ms(),
              publisher_plugin_id="com.example.x", payload={})
    )
    await bus.publish(
        Event(topic="plugin.com.example.x.health", timestamp_ms=now_ms(),
              publisher_plugin_id="com.example.x", payload={})
    )
    # Non-matching publish must not be delivered to this subscriber.
    await bus.publish(
        Event(topic="plugin.com.other.y.alert", timestamp_ms=now_ms(),
              publisher_plugin_id="com.other.y", payload={})
    )
    await asyncio.wait_for(task, timeout=1.0)
    assert {e.topic for e in got} == {
        "plugin.com.example.x.alert",
        "plugin.com.example.x.health",
    }


@pytest.mark.asyncio
async def test_subscribe_iterator_cleanup_on_explicit_close() -> None:
    bus = EventBus()
    gen = bus.subscribe("vehicle.armed")
    aiter = gen.__aiter__()
    # Kick the generator to register before publishing. The body runs
    # only on first __anext__, so we start it in a task and yield.
    fetch = asyncio.create_task(aiter.__anext__())
    for _ in range(10):
        if bus.subscriber_count() == 1:
            break
        await asyncio.sleep(0)
    assert bus.subscriber_count() == 1
    await bus.publish(
        Event(topic="vehicle.armed", timestamp_ms=now_ms(),
              publisher_plugin_id=None, payload={})
    )
    received = await asyncio.wait_for(fetch, timeout=1.0)
    assert received.topic == "vehicle.armed"
    # Explicit close runs the generator's finally block, which removes
    # the queue from the registry.
    await aiter.aclose()
    assert bus.subscriber_count() == 0


# ---------------------------------------------------------------------
# Capability gates
# ---------------------------------------------------------------------


def test_subscribe_requires_event_subscribe_capability() -> None:
    assert not is_subscribe_allowed(
        plugin_id="com.example.x",
        topic_pattern="vehicle.armed",
        granted_caps=set(),
    )


def test_subscribe_public_topic_passes_with_capability() -> None:
    assert is_subscribe_allowed(
        plugin_id="com.example.x",
        topic_pattern="vehicle.armed",
        granted_caps={"event.subscribe"},
    )


def test_subscribe_own_namespace_always_passes_with_capability() -> None:
    assert is_subscribe_allowed(
        plugin_id="com.example.x",
        topic_pattern="plugin.com.example.x.health",
        granted_caps={"event.subscribe"},
    )


def test_subscribe_other_plugin_namespace_blocked() -> None:
    # Without an extra-allow entry, a plugin cannot subscribe to a peer's namespace.
    assert not is_subscribe_allowed(
        plugin_id="com.example.x",
        topic_pattern="plugin.com.other.y.health",
        granted_caps={"event.subscribe"},
    )


def test_subscribe_extra_allowlist_unblocks() -> None:
    assert is_subscribe_allowed(
        plugin_id="com.example.x",
        topic_pattern="plugin.com.other.y.health",
        granted_caps={"event.subscribe"},
        extra_allow={"plugin.com.other.y.*"},
    )


def test_publish_own_namespace_always_allowed() -> None:
    assert is_publish_allowed(
        plugin_id="com.example.x",
        topic="plugin.com.example.x.alert",
        granted_caps=set(),
    )


def test_publish_reserved_namespace_blocked_even_with_capability() -> None:
    # vehicle.* is host-owned. event.publish does not unlock it.
    assert not is_publish_allowed(
        plugin_id="com.example.x",
        topic="vehicle.armed",
        granted_caps={"event.publish"},
    )


def test_publish_custom_topic_requires_capability() -> None:
    # Non-reserved, non-namespaced topic. Capability required.
    assert not is_publish_allowed(
        plugin_id="com.example.x",
        topic="payload.released",
        granted_caps=set(),
    )
    assert is_publish_allowed(
        plugin_id="com.example.x",
        topic="payload.released",
        granted_caps={"event.publish"},
    )
