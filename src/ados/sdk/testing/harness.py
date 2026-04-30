""":class:`PluginTestHarness` — the public surface plugin authors use.

Designed to feel like a typical async-pytest fixture:

.. code-block:: python

    import pytest
    from ados.sdk.testing import PluginTestHarness

    @pytest.fixture
    async def harness():
        async with PluginTestHarness(
            plugin_id="com.example.thermal",
            plugin_version="1.0.0",
            granted_capabilities={"event.publish", "event.subscribe"},
        ) as h:
            yield h

    @pytest.mark.asyncio
    async def test_alerts_on_high_temp(harness):
        plugin = ThermalPlugin()
        await plugin.on_start(harness.context)
        await harness.publish_event("telemetry.thermal", {"max_c": 95})
        assert any(
            t == "alert.thermal" for t, _ in harness.published_events()
        )
"""

from __future__ import annotations

import asyncio
from pathlib import Path
from typing import Any

from ados.plugins.ipc_client import PluginContext
from ados.sdk.testing.fixtures import FixtureEvent, load_fixture
from ados.sdk.testing.stubs import FakeIpcClient

PublishedEvent = tuple[str, dict[str, Any]]


class PluginTestHarness:
    """Wires a :class:`PluginContext` to in-process stubs.

    Use as an async context manager. Inside the ``async with`` block,
    ``harness.context`` is the :class:`PluginContext` to pass to the
    plugin's lifecycle hooks. ``harness.published_events()`` returns
    the list of ``(topic, payload)`` tuples the plugin published.
    """

    def __init__(
        self,
        *,
        plugin_id: str,
        plugin_version: str = "0.0.0",
        config: dict[str, Any] | None = None,
        granted_capabilities: set[str] | None = None,
        fixtures_root: str | Path | None = None,
        named_fixtures: dict[str, str] | None = None,
    ) -> None:
        self.plugin_id = plugin_id
        self.plugin_version = plugin_version
        self.config: dict[str, Any] = dict(config or {})
        self._ipc = FakeIpcClient(
            plugin_id=plugin_id,
            granted_capabilities=set(granted_capabilities or set()),
        )
        self.context = PluginContext(
            plugin_id=plugin_id,
            plugin_version=plugin_version,
            config=self.config,
            ipc=self._ipc,  # type: ignore[arg-type]
        )
        self._fixtures_root = Path(fixtures_root) if fixtures_root else None
        self._named_fixtures = dict(named_fixtures or {})

    async def __aenter__(self) -> "PluginTestHarness":
        return self

    async def __aexit__(self, *_: object) -> None:
        await self._ipc.close()

    # ------------------------------------------------------------------
    # Capability injection
    # ------------------------------------------------------------------

    def grant(self, *capabilities: str) -> None:
        for cap in capabilities:
            self._ipc.grant(cap)

    def revoke(self, *capabilities: str) -> None:
        for cap in capabilities:
            self._ipc.revoke(cap)

    @property
    def granted_capabilities(self) -> frozenset[str]:
        return self._ipc.granted_capabilities

    # ------------------------------------------------------------------
    # Event injection + capture
    # ------------------------------------------------------------------

    async def publish_event(
        self, topic: str, payload: dict[str, Any] | None = None
    ) -> int:
        """Deliver an event to the plugin's subscribers as if a peer published it."""
        return await self._ipc.deliver(topic, payload or {})

    def published_events(self) -> list[PublishedEvent]:
        """Events the plugin under test published. Returns a copy."""
        return list(self._ipc.published)

    def clear_published(self) -> None:
        self._ipc.published.clear()

    # ------------------------------------------------------------------
    # Fixture replay
    # ------------------------------------------------------------------

    async def replay_fixture(self, name_or_path: str | Path) -> int:
        """Replay events from a fixture YAML.

        ``name_or_path`` resolves in this order:
        1. A name registered via ``named_fixtures`` (from the manifest).
        2. A path relative to ``fixtures_root`` if set.
        3. A literal path.
        """
        path = self._resolve_fixture(name_or_path)
        events = load_fixture(path)
        return await self.replay_events(events)

    async def replay_events(self, events: list[FixtureEvent]) -> int:
        """Replay an explicit event list. Honors ``delay_ms`` between events."""
        delivered = 0
        for ev in events:
            if ev.delay_ms > 0:
                await asyncio.sleep(ev.delay_ms / 1000.0)
            delivered += await self._ipc.deliver(ev.topic, ev.payload)
        return delivered

    def _resolve_fixture(self, name_or_path: str | Path) -> Path:
        key = str(name_or_path)
        if key in self._named_fixtures:
            mapped = self._named_fixtures[key]
            if self._fixtures_root is not None:
                return self._fixtures_root / mapped
            return Path(mapped)
        candidate = Path(name_or_path)
        if self._fixtures_root is not None and not candidate.is_absolute():
            rooted = self._fixtures_root / candidate
            if rooted.exists():
                return rooted
        return candidate
