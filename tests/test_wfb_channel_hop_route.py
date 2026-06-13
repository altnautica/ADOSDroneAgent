"""POST /api/wfb/channel forwards a coordinated hop to the native radio socket.

On the native transmit plane there is no in-process Python WFB manager, so the
route must forward ``{"op":"hop","channel":N}`` to the radio command socket and
return the socket's verdict. These tests pin: an accepted hop returns 200 with
the channel; a refused hop surfaces the reason as a 409; an unreachable socket
falls through to the packaged/demo path; and an invalid channel is still a 400
before the socket is ever touched. One test drives a real ephemeral unix socket
to prove the wire contract (`{"op":"hop","channel":N}` in, `{"ok":bool}` out).
"""

from __future__ import annotations

import asyncio
import json
import threading
from pathlib import Path
from tempfile import mkdtemp
from typing import Any
from unittest.mock import MagicMock

import pytest
from fastapi.testclient import TestClient

from ados.api.routes import wfb as wfb_mod
from ados.api.server import create_app
from ados.core.config import ADOSConfig
from tests.api_runtime_utils import build_api_runtime


def _client() -> TestClient:
    runtime: Any = build_api_runtime(config=ADOSConfig())
    return TestClient(create_app(runtime))


def test_invalid_channel_is_400_before_socket(monkeypatch):
    """An out-of-set channel is rejected before any socket forward."""
    forwarded = {"hit": False}

    async def _never(_ch):
        forwarded["hit"] = True
        return {"ok": True, "channel": 999}

    monkeypatch.setattr(wfb_mod, "_native_radio_running", lambda: True)
    monkeypatch.setattr(wfb_mod, "_radio_hop_via_socket", _never)

    resp = _client().post("/api/wfb/channel", json={"channel": 999})
    assert resp.status_code == 400
    assert "Invalid channel" in resp.json()["detail"]
    assert forwarded["hit"] is False


def test_native_hop_accepted_returns_200(monkeypatch):
    """An accepted hop returns 200 with the channel + frequency."""

    async def _accept(ch):
        return {"ok": True, "channel": ch}

    monkeypatch.setattr(wfb_mod, "_native_radio_running", lambda: True)
    monkeypatch.setattr(wfb_mod, "_radio_hop_via_socket", _accept)

    resp = _client().post("/api/wfb/channel", json={"channel": 157})
    assert resp.status_code == 200, resp.text
    body = resp.json()
    assert body["status"] == "ok"
    assert body["channel"] == 157
    assert body["frequency_mhz"] == 5785


def test_native_hop_refused_returns_409(monkeypatch):
    """A refused hop (e.g. no peer) surfaces the reason as a 409."""

    async def _refuse(_ch):
        return {"ok": False, "error": "no peer"}

    monkeypatch.setattr(wfb_mod, "_native_radio_running", lambda: True)
    monkeypatch.setattr(wfb_mod, "_radio_hop_via_socket", _refuse)

    resp = _client().post("/api/wfb/channel", json={"channel": 149})
    assert resp.status_code == 409
    assert resp.json()["detail"]["message"] == "no peer"


def test_native_socket_unreachable_falls_through_to_503(monkeypatch):
    """When the native socket is unreachable AND no packaged manager exists, the
    route falls through to the existing 503 instead of pretending success."""

    async def _unreachable(_ch):
        return None

    monkeypatch.setattr(wfb_mod, "_native_radio_running", lambda: True)
    monkeypatch.setattr(wfb_mod, "_radio_hop_via_socket", _unreachable)

    # build_api_runtime ships no wfb manager → the fall-through hits the 503.
    resp = _client().post("/api/wfb/channel", json={"channel": 149})
    assert resp.status_code == 503


def test_non_native_uses_packaged_manager(monkeypatch):
    """When the radio is not native, the demo/packaged manager path runs."""
    monkeypatch.setattr(wfb_mod, "_native_radio_running", lambda: False)

    fake_wfb = MagicMock()
    runtime: Any = build_api_runtime(config=ADOSConfig(), wfb_manager=fake_wfb)
    client = TestClient(create_app(runtime))

    resp = client.post("/api/wfb/channel", json={"channel": 36})
    assert resp.status_code == 200, resp.text
    assert resp.json()["channel"] == 36
    assert fake_wfb._channel == 36


class _HopSocketServer:
    """A real ephemeral unix-socket server speaking the radio hop contract."""

    def __init__(self, sock_path: Path, reply: dict) -> None:
        self.sock_path = sock_path
        self.reply = reply
        self.requests: list[dict] = []
        self._loop: asyncio.AbstractEventLoop | None = None
        self._thread: threading.Thread | None = None
        self._server: asyncio.AbstractServer | None = None

    async def _handle(self, reader, writer):
        line = await reader.readline()
        try:
            self.requests.append(json.loads(line.decode()))
        except ValueError:
            self.requests.append({"_raw": line.decode(errors="replace")})
        writer.write((json.dumps(self.reply) + "\n").encode())
        await writer.drain()
        writer.close()

    def start(self):
        ready = threading.Event()

        def _run():
            loop = asyncio.new_event_loop()
            asyncio.set_event_loop(loop)
            self._loop = loop
            self._server = loop.run_until_complete(
                asyncio.start_unix_server(self._handle, path=str(self.sock_path))
            )
            ready.set()
            loop.run_forever()

        self._thread = threading.Thread(target=_run, daemon=True)
        self._thread.start()
        assert ready.wait(5.0), "hop socket server did not start"

    def stop(self):
        if self._loop:
            self._loop.call_soon_threadsafe(self._loop.stop)
        if self._thread:
            self._thread.join(timeout=2.0)


def test_real_socket_roundtrip_wire_contract(monkeypatch):
    """End-to-end over a real unix socket: the request is exactly
    {"op":"hop","channel":N} and an {"ok":true,"channel":N} reply returns 200."""
    sock_path = Path(mkdtemp(prefix="ados-hop-")) / "radio-cmd.sock"
    server = _HopSocketServer(sock_path, {"ok": True, "channel": 161})
    server.start()
    try:
        monkeypatch.setattr(wfb_mod, "_native_radio_running", lambda: True)
        monkeypatch.setattr(wfb_mod, "RADIO_CMD_SOCK", sock_path)

        resp = _client().post("/api/wfb/channel", json={"channel": 161})
        assert resp.status_code == 200, resp.text
        assert resp.json()["channel"] == 161
        assert server.requests == [{"op": "hop", "channel": 161}]
    finally:
        server.stop()


@pytest.mark.asyncio
async def test_radio_hop_via_socket_returns_none_when_unreachable(monkeypatch, tmp_path):
    """The socket helper returns None (not an exception) for an absent socket."""
    monkeypatch.setattr(wfb_mod, "RADIO_CMD_SOCK", tmp_path / "nope.sock")
    result = await wfb_mod._radio_hop_via_socket(149)
    assert result is None
