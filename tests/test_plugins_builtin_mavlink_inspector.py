"""Built-in MAVLink inspector plugin tests.

Walks the manifest shape, exercises the lifecycle hooks against a
stub :class:`PluginContext`, and confirms each subscribed event
folds into the running snapshot and is republished under the
plugin's own namespace.
"""

from __future__ import annotations

import asyncio
from collections.abc import Awaitable, Callable
from typing import Any

import pytest

from ados.plugins.builtin.mavlink_inspector import (
    PLUGIN_ID,
    SUBSCRIBED_TOPICS,
    MavlinkInspectorPlugin,
    get_manifest,
    manifest as module_manifest,
)
from ados.plugins.manifest import PluginManifest


def test_get_manifest_returns_plugin_manifest() -> None:
    m = get_manifest()
    assert isinstance(m, PluginManifest)
    assert m.id == PLUGIN_ID
    assert m.agent is not None
    assert m.agent.isolation == "inprocess"
    assert {p.id for p in m.agent.permissions} == {
        "event.publish",
        "event.subscribe",
    }


def test_module_manifest_attribute_matches_callable() -> None:
    """Loader probes ``.manifest`` first; both paths must agree."""
    assert isinstance(module_manifest, PluginManifest)
    assert module_manifest.id == get_manifest().id


def test_loader_picks_up_mavlink_inspector_entry_point() -> None:
    from ados.plugins.loader import load_builtin_manifests

    manifests = load_builtin_manifests()
    ids = [m.id for m in manifests]
    assert PLUGIN_ID in ids


# ---------------------------------------------------------------------
# Lifecycle hooks against a stub context
# ---------------------------------------------------------------------


class _StubEvents:
    def __init__(self) -> None:
        self.published: list[tuple[str, dict[str, Any]]] = []
        self.subscriptions: dict[
            str, list[Callable[[dict[str, Any]], Awaitable[None] | None]]
        ] = {}

    async def publish(self, topic: str, payload: dict) -> int:
        self.published.append((topic, payload))
        return 1

    async def subscribe(
        self,
        topic_pattern: str,
        cb: Callable[[dict[str, Any]], Awaitable[None] | None],
    ) -> None:
        self.subscriptions.setdefault(topic_pattern, []).append(cb)


class _StubLog:
    def __init__(self) -> None:
        self.records: list[tuple[str, dict]] = []

    def info(self, event: str, **kw: Any) -> None:
        self.records.append((event, kw))

    def warning(self, event: str, **kw: Any) -> None:
        self.records.append((event, kw))

    def error(self, event: str, **kw: Any) -> None:
        self.records.append((event, kw))


class _StubContext:
    def __init__(self) -> None:
        self.plugin_id = PLUGIN_ID
        self.plugin_version = "0.1.0"
        self.config: dict = {}
        self.events = _StubEvents()
        self.log = _StubLog()


@pytest.mark.asyncio
async def test_on_start_subscribes_to_expected_topics() -> None:
    plugin = MavlinkInspectorPlugin()
    ctx = _StubContext()
    await plugin.on_start(ctx)
    for topic in SUBSCRIBED_TOPICS:
        assert topic in ctx.events.subscriptions
    assert any(rec[0] == "mavlink_inspector_started" for rec in ctx.log.records)


@pytest.mark.asyncio
async def test_armed_callback_updates_state_and_publishes_snapshot() -> None:
    plugin = MavlinkInspectorPlugin()
    ctx = _StubContext()
    await plugin.on_start(ctx)

    cb = ctx.events.subscriptions["vehicle.armed"][0]
    result = cb({"by": "operator"})
    if asyncio.iscoroutine(result):
        await result

    assert plugin._state["armed"] is True
    assert plugin._state["last_event_ts_ms"] is not None
    assert ctx.events.published, "expected snapshot publish"
    topic, payload = ctx.events.published[-1]
    assert topic == f"plugin.{PLUGIN_ID}.snapshot"
    assert payload["armed"] is True


@pytest.mark.asyncio
async def test_mode_changed_callback_updates_mode() -> None:
    plugin = MavlinkInspectorPlugin()
    ctx = _StubContext()
    await plugin.on_start(ctx)

    cb = ctx.events.subscriptions["vehicle.mode_changed"][0]
    result = cb({"mode": "AUTO"})
    if asyncio.iscoroutine(result):
        await result

    assert plugin._state["mode"] == "AUTO"
    topic, payload = ctx.events.published[-1]
    assert topic == f"plugin.{PLUGIN_ID}.snapshot"
    assert payload["mode"] == "AUTO"


@pytest.mark.asyncio
async def test_battery_low_callback_updates_battery_pct() -> None:
    plugin = MavlinkInspectorPlugin()
    ctx = _StubContext()
    await plugin.on_start(ctx)

    cb = ctx.events.subscriptions["vehicle.battery_low"][0]
    result = cb({"battery_pct": 17.5})
    if asyncio.iscoroutine(result):
        await result

    assert plugin._state["battery_pct"] == 17.5
    topic, payload = ctx.events.published[-1]
    assert topic == f"plugin.{PLUGIN_ID}.snapshot"
    assert payload["battery_pct"] == 17.5


@pytest.mark.asyncio
async def test_disarmed_callback_clears_armed_flag() -> None:
    plugin = MavlinkInspectorPlugin()
    ctx = _StubContext()
    await plugin.on_start(ctx)

    armed_cb = ctx.events.subscriptions["vehicle.armed"][0]
    armed_result = armed_cb({})
    if asyncio.iscoroutine(armed_result):
        await armed_result

    disarmed_cb = ctx.events.subscriptions["vehicle.disarmed"][0]
    disarmed_result = disarmed_cb({})
    if asyncio.iscoroutine(disarmed_result):
        await disarmed_result

    assert plugin._state["armed"] is False
    topic, payload = ctx.events.published[-1]
    assert topic == f"plugin.{PLUGIN_ID}.snapshot"
    assert payload["armed"] is False


@pytest.mark.asyncio
async def test_on_stop_logs_cleanly() -> None:
    plugin = MavlinkInspectorPlugin()
    ctx = _StubContext()
    await plugin.on_stop(ctx)
    assert any(rec[0] == "mavlink_inspector_stopped" for rec in ctx.log.records)
