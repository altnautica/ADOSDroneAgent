"""Built-in telemetry logger plugin tests.

Walks the manifest shape, exercises the lifecycle hooks against a
stub :class:`PluginContext`, and confirms a structured log line is
emitted per public-topic event.
"""

from __future__ import annotations

import asyncio
from collections.abc import Awaitable, Callable
from typing import Any

import pytest

from ados.plugins.builtin.telemetry_logger import (
    PLUGIN_ID,
    PUBLIC_TOPICS,
    TelemetryLoggerPlugin,
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
    assert {p.id for p in m.agent.permissions} == {"event.subscribe"}


def test_module_manifest_attribute_matches_callable() -> None:
    """Loader probes ``.manifest`` first; both paths must agree."""
    assert isinstance(module_manifest, PluginManifest)
    assert module_manifest.id == get_manifest().id


def test_loader_picks_up_telemetry_logger_entry_point() -> None:
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
async def test_on_start_subscribes_to_every_public_topic() -> None:
    plugin = TelemetryLoggerPlugin()
    ctx = _StubContext()
    await plugin.on_start(ctx)
    for topic in PUBLIC_TOPICS:
        assert topic in ctx.events.subscriptions
    assert any(rec[0] == "telemetry_logger_started" for rec in ctx.log.records)


@pytest.mark.asyncio
async def test_callback_emits_structured_event_log() -> None:
    plugin = TelemetryLoggerPlugin()
    ctx = _StubContext()
    await plugin.on_start(ctx)
    cb = ctx.events.subscriptions["vehicle.armed"][0]
    payload = {"by": "operator", "ts_ms": 1700000000000}
    result = cb(payload)
    if asyncio.iscoroutine(result):
        await result
    matching = [
        rec
        for rec in ctx.log.records
        if rec[0] == "telemetry_event" and rec[1].get("topic") == "vehicle.armed"
    ]
    assert len(matching) == 1
    assert matching[0][1]["payload"] == payload


@pytest.mark.asyncio
async def test_on_stop_logs_cleanly() -> None:
    plugin = TelemetryLoggerPlugin()
    ctx = _StubContext()
    await plugin.on_stop(ctx)
    assert any(rec[0] == "telemetry_logger_stopped" for rec in ctx.log.records)
