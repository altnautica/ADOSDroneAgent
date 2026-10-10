"""Driver registration names the catalog capability for its kind."""

from __future__ import annotations

import asyncio
from pathlib import Path
from types import SimpleNamespace

import pytest

from ados.plugins.ipc_client import PluginIpcClient, _driver_kind_capability


def _client(sent: list[tuple[str, str, dict]]) -> PluginIpcClient:
    client = PluginIpcClient(
        plugin_id="com.example.demo",
        token="tok",
        socket_path=Path("/run/ados/plugins/x.sock"),
    )

    async def fake_send(method: str, *, capability: str, args: dict, **_: object):
        sent.append((method, capability, args))
        return SimpleNamespace(args={"handle_id": "h1"})

    client._send_request = fake_send  # type: ignore[method-assign]
    return client


@pytest.mark.parametrize(
    ("kind", "cap"),
    [
        ("camera", "sensor.camera.register"),
        ("lidar", "sensor.lidar.register"),
        ("gimbal", "sensor.payload.register"),
        ("gps", "sensor.payload.register"),
        ("esc", "sensor.payload.register"),
        ("payload-actuator", "sensor.payload.register"),
    ],
)
def test_driver_kind_sends_catalog_cap(kind: str, cap: str) -> None:
    sent: list[tuple[str, str, dict]] = []
    reply = asyncio.run(_client(sent).peripheral_register_driver(kind, "ref-1"))
    assert reply == {"handle_id": "h1"}
    assert sent == [
        ("peripheral.register_driver", cap, {"kind": kind, "driver_ref": "ref-1"})
    ]


def test_unknown_driver_kind_raises_and_sends_nothing() -> None:
    sent: list[tuple[str, str, dict]] = []
    with pytest.raises(ValueError, match="sonar"):
        asyncio.run(_client(sent).peripheral_register_driver("sonar", "ref-1"))
    assert sent == []


def test_every_mapped_cap_is_in_the_catalog() -> None:
    from ados.plugins.capabilities import is_known_agent_capability

    for kind in ("camera", "lidar", "gimbal", "gps", "esc", "payload-actuator"):
        assert is_known_agent_capability(_driver_kind_capability(kind))
