"""The Python telemetry subscription matches the Rust SDK's wire."""

from __future__ import annotations

import asyncio
from pathlib import Path
from types import SimpleNamespace

import pytest

from ados.plugins.ipc_client import TELEMETRY_STATE_TOPIC, PluginIpcClient
from ados.plugins.rpc import Envelope


def _client(sent: list[tuple[str, str, dict]]) -> PluginIpcClient:
    client = PluginIpcClient(
        plugin_id="com.example.demo",
        token="tok",
        socket_path=Path("/run/ados/plugins/x.sock"),
    )

    async def fake_send(method: str, *, capability: str, args: dict, **_: object):
        sent.append((method, capability, args))
        return SimpleNamespace(args={})

    client._send_request = fake_send  # type: ignore[method-assign]
    return client


def _state_event(payload: dict) -> Envelope:
    return Envelope(
        type="event",
        method="event.deliver",
        capability="",
        args={"topic": TELEMETRY_STATE_TOPIC, "payload": payload},
        request_id="",
        token="",
    )


async def test_subscribe_telemetry_capability() -> None:
    sent: list[tuple[str, str, dict]] = []
    client = _client(sent)
    stream = client.subscribe_telemetry()
    first = asyncio.ensure_future(stream.__anext__())
    await asyncio.sleep(0)
    assert sent == [("telemetry.subscribe", "telemetry.read", {})]

    await client._dispatch_event(_state_event({"armed": False, "mode": "QLOITER"}))
    assert await asyncio.wait_for(first, 1.0) == {"armed": False, "mode": "QLOITER"}

    # A slow consumer sees the newest snapshot, not a backlog.
    await client._dispatch_event(_state_event({"seq": 1}))
    await client._dispatch_event(_state_event({"seq": 2}))
    assert await asyncio.wait_for(stream.__anext__(), 1.0) == {"seq": 2}

    await stream.aclose()
    assert client._event_callbacks[TELEMETRY_STATE_TOPIC] == []


async def test_subscribe_telemetry_refuses_an_unknown_topic() -> None:
    sent: list[tuple[str, str, dict]] = []
    with pytest.raises(ValueError):
        await _client(sent).subscribe_telemetry("telemetry.attitude").__anext__()
    assert sent == []
