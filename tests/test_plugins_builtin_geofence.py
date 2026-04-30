"""Built-in geofence plugin tests.

Walks the manifest shape, exercises the lifecycle hooks against a
stub :class:`PluginContext`, and confirms the alert republish goes
out under the plugin's own namespace.
"""

from __future__ import annotations

import asyncio
from collections.abc import Awaitable, Callable
from typing import Any

import pytest

from ados.plugins.builtin.geofence import (
    PLUGIN_ID,
    GeofencePlugin,
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


def test_loader_picks_up_geofence_entry_point() -> None:
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
async def test_on_start_subscribes_to_geofence_breach() -> None:
    plugin = GeofencePlugin()
    ctx = _StubContext()
    await plugin.on_start(ctx)
    assert "vehicle.geofence_breach" in ctx.events.subscriptions


@pytest.mark.asyncio
async def test_breach_callback_republishes_alert() -> None:
    plugin = GeofencePlugin()
    ctx = _StubContext()
    await plugin.on_start(ctx)
    cb = ctx.events.subscriptions["vehicle.geofence_breach"][0]
    result = cb(
        {
            "fence": "F-01",
            "severity": "critical",
            "lat": 12.97,
            "lon": 77.59,
            "alt_m_agl": 42.0,
        }
    )
    if asyncio.iscoroutine(result):
        await result
    assert ctx.events.published == [
        (
            f"plugin.{PLUGIN_ID}.alert",
            {
                "kind": "geofence_breach",
                "fence": "F-01",
                "severity": "critical",
                "lat": 12.97,
                "lon": 77.59,
                "alt_m_agl": 42.0,
            },
        )
    ]


@pytest.mark.asyncio
async def test_on_stop_logs_cleanly() -> None:
    plugin = GeofencePlugin()
    ctx = _StubContext()
    await plugin.on_stop(ctx)
    assert any(rec[0] == "geofence_plugin_stopped" for rec in ctx.log.records)
