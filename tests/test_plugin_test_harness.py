"""PluginTestHarness coverage.

Shape of the suite mirrors what plugin authors will write against the
public SDK surface. Each test exercises one harness behavior: lifecycle
context manager, capability gate, event capture, event injection,
fixture replay (inline and file).
"""

from __future__ import annotations

from pathlib import Path

import pytest

from ados.plugins.errors import CapabilityDenied
from ados.sdk.testing import (
    FixtureEvent,
    PluginTestHarness,
    load_fixture,
)


PLUGIN_ID = "com.example.harness"


@pytest.mark.asyncio
async def test_context_manager_yields_a_wired_context() -> None:
    async with PluginTestHarness(plugin_id=PLUGIN_ID) as h:
        assert h.context.plugin_id == PLUGIN_ID
        assert h.context.plugin_version == "0.0.0"
        # ping_supervisor should round-trip through the fake client.
        result = await h.context.ping_supervisor()
        assert result["pong"] is True
        assert result["plugin_id"] == PLUGIN_ID


@pytest.mark.asyncio
async def test_publish_requires_event_publish_capability() -> None:
    async with PluginTestHarness(plugin_id=PLUGIN_ID) as h:
        with pytest.raises(CapabilityDenied) as excinfo:
            await h.context.events.publish("topic.x", {"k": 1})
        assert excinfo.value.capability == "event.publish"


@pytest.mark.asyncio
async def test_subscribe_requires_event_subscribe_capability() -> None:
    async with PluginTestHarness(plugin_id=PLUGIN_ID) as h:
        with pytest.raises(CapabilityDenied) as excinfo:
            await h.context.events.subscribe("topic.x", lambda _p: None)
        assert excinfo.value.capability == "event.subscribe"


@pytest.mark.asyncio
async def test_grant_unlocks_publish_and_capture_records_topic() -> None:
    async with PluginTestHarness(
        plugin_id=PLUGIN_ID,
        granted_capabilities={"event.publish"},
    ) as h:
        await h.context.events.publish("alert.thermal", {"max_c": 95})
        captured = h.published_events()
        assert captured == [("alert.thermal", {"max_c": 95})]


@pytest.mark.asyncio
async def test_publish_event_injects_into_subscriber() -> None:
    received: list[dict] = []

    async with PluginTestHarness(
        plugin_id=PLUGIN_ID,
        granted_capabilities={"event.subscribe"},
    ) as h:
        async def cb(payload: dict) -> None:
            received.append(payload)

        await h.context.events.subscribe("telemetry.battery", cb)
        delivered = await h.publish_event(
            "telemetry.battery", {"voltage_mv": 24800}
        )
        assert delivered == 1
        assert received == [{"voltage_mv": 24800}]


@pytest.mark.asyncio
async def test_grant_after_construction() -> None:
    async with PluginTestHarness(plugin_id=PLUGIN_ID) as h:
        assert "event.publish" not in h.granted_capabilities
        h.grant("event.publish")
        await h.context.events.publish("ok", {})
        assert h.published_events() == [("ok", {})]
        h.revoke("event.publish")
        with pytest.raises(CapabilityDenied):
            await h.context.events.publish("nope", {})


@pytest.mark.asyncio
async def test_replay_events_from_explicit_list() -> None:
    seen: list[str] = []

    async with PluginTestHarness(
        plugin_id=PLUGIN_ID,
        granted_capabilities={"event.subscribe"},
    ) as h:
        await h.context.events.subscribe("telemetry.*", lambda p: seen.append(p["k"]))
        await h.replay_events(
            [
                FixtureEvent(topic="telemetry.battery", payload={"k": "battery"}),
                FixtureEvent(topic="telemetry.gps", payload={"k": "gps"}),
            ]
        )
        assert seen == ["battery", "gps"]


@pytest.mark.asyncio
async def test_replay_fixture_from_yaml(tmp_path: Path) -> None:
    fixture_path = tmp_path / "scenario.yaml"
    fixture_path.write_text(
        "- topic: telemetry.battery\n"
        "  payload: {voltage_mv: 24800}\n"
        "- topic: telemetry.gps\n"
        "  payload: {fix_type: 3}\n",
        encoding="utf-8",
    )
    seen: list[tuple[str, dict]] = []

    async with PluginTestHarness(
        plugin_id=PLUGIN_ID,
        granted_capabilities={"event.subscribe"},
    ) as h:
        await h.context.events.subscribe(
            "telemetry.*",
            lambda p: seen.append((p.get("voltage_mv") or p.get("fix_type"), p)),
        )
        delivered = await h.replay_fixture(fixture_path)
        assert delivered == 2


@pytest.mark.asyncio
async def test_named_fixture_resolves_against_root(tmp_path: Path) -> None:
    root = tmp_path / "fixtures"
    root.mkdir()
    (root / "happy.yaml").write_text(
        "- topic: x\n  payload: {ok: true}\n", encoding="utf-8"
    )

    async with PluginTestHarness(
        plugin_id=PLUGIN_ID,
        granted_capabilities={"event.subscribe"},
        fixtures_root=root,
        named_fixtures={"happy": "happy.yaml"},
    ) as h:
        seen: list[dict] = []
        await h.context.events.subscribe("x", lambda p: seen.append(p))
        await h.replay_fixture("happy")
        assert seen == [{"ok": True}]


def test_load_fixture_rejects_non_list(tmp_path: Path) -> None:
    p = tmp_path / "bad.yaml"
    p.write_text("topic: x\npayload: {}\n", encoding="utf-8")
    with pytest.raises(Exception):
        load_fixture(p)


def test_load_fixture_returns_empty_for_empty_file(tmp_path: Path) -> None:
    p = tmp_path / "empty.yaml"
    p.write_text("", encoding="utf-8")
    assert load_fixture(p) == []
