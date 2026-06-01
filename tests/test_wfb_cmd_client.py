"""Tests for the native radio command-socket client.

``cmd_client`` forwards the operator radio knobs (TX power, FEC, MCS, link
tier) to the native transmit plane's command socket at
``/run/ados/wfb-cmd.sock`` when the native radio is the running
implementation. These tests cover the unreachable-socket path (which the
REST layer treats as "fall back to the packaged manager"), the
server-reported failure path, and a happy-path round-trip against a tiny
in-test Unix listener that mirrors the radio's newline-JSON framing.
"""

from __future__ import annotations

import asyncio
import json
from pathlib import Path

import pytest

from ados.services.wfb import cmd_client
from ados.services.wfb.cmd_client import (
    RadioCmdError,
    RadioCmdUnavailableError,
)


@pytest.mark.asyncio
async def test_unreachable_socket_raises_unavailable(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """An absent command socket raises RadioCmdUnavailableError so the REST
    layer can fall back to the packaged manager."""
    monkeypatch.setattr(
        cmd_client, "WFB_CMD_SOCK", str(tmp_path / "does-not-exist.sock")
    )
    with pytest.raises(RadioCmdUnavailableError):
        await cmd_client.set_fec(8, 12)


async def _serve_one_reply(sock_path: str, reply: dict) -> asyncio.AbstractServer:
    """A Unix listener that answers one request per connection: read the
    newline-terminated request, write ``reply`` as one newline-terminated
    JSON line, then close."""

    async def _handle(
        reader: asyncio.StreamReader, writer: asyncio.StreamWriter
    ) -> None:
        await reader.readline()
        writer.write((json.dumps(reply) + "\n").encode("utf-8"))
        await writer.drain()
        writer.close()

    return await asyncio.start_unix_server(_handle, path=sock_path)


async def _capture_one_request(
    sock_path: str, reply: dict, sink: list[dict]
) -> asyncio.AbstractServer:
    """Like ``_serve_one_reply`` but records the request the client sent into
    ``sink`` so a test can assert the wire shape."""

    async def _handle(
        reader: asyncio.StreamReader, writer: asyncio.StreamWriter
    ) -> None:
        line = await reader.readline()
        sink.append(json.loads(line.decode("utf-8")))
        writer.write((json.dumps(reply) + "\n").encode("utf-8"))
        await writer.drain()
        writer.close()

    return await asyncio.start_unix_server(_handle, path=sock_path)


@pytest.mark.asyncio
async def test_set_tx_power_round_trips_effective_dbm(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """A successful TX-power reply returns the effective dBm the driver
    accepted."""
    sock_path = str(tmp_path / "wfb-cmd.sock")
    monkeypatch.setattr(cmd_client, "WFB_CMD_SOCK", sock_path)
    sink: list[dict] = []
    server = await _capture_one_request(
        sock_path, {"ok": True, "effective_dbm": 10}, sink
    )
    try:
        effective = await cmd_client.set_tx_power(8)
        assert effective == 10
        # The wire request carries the op + the requested dBm.
        assert sink == [{"op": "set_tx_power", "tx_power_dbm": 8}]
    finally:
        server.close()
        await server.wait_closed()


@pytest.mark.asyncio
async def test_set_tx_power_null_effective_when_all_rejected(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """A null ``effective_dbm`` (every ramp step rejected) maps to None."""
    sock_path = str(tmp_path / "wfb-cmd.sock")
    monkeypatch.setattr(cmd_client, "WFB_CMD_SOCK", sock_path)
    server = await _serve_one_reply(sock_path, {"ok": True, "effective_dbm": None})
    try:
        assert await cmd_client.set_tx_power(5) is None
    finally:
        server.close()
        await server.wait_closed()


@pytest.mark.asyncio
async def test_failed_apply_raises_cmd_error(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """An ``ok: false`` reply raises RadioCmdError carrying the server code."""
    sock_path = str(tmp_path / "wfb-cmd.sock")
    monkeypatch.setattr(cmd_client, "WFB_CMD_SOCK", sock_path)
    server = await _serve_one_reply(
        sock_path, {"ok": False, "error": "E_SET_FEC_FAILED"}
    )
    try:
        with pytest.raises(RadioCmdError) as exc:
            await cmd_client.set_fec(8, 12)
        assert "E_SET_FEC_FAILED" in str(exc.value)
    finally:
        server.close()
        await server.wait_closed()


@pytest.mark.asyncio
async def test_set_tier_manual_sends_full_trio(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """The manual tier request carries the mode + the full mcs/fec trio."""
    sock_path = str(tmp_path / "wfb-cmd.sock")
    monkeypatch.setattr(cmd_client, "WFB_CMD_SOCK", sock_path)
    sink: list[dict] = []
    server = await _capture_one_request(sock_path, {"ok": True}, sink)
    try:
        await cmd_client.set_tier_manual(5, 8, 10)
        assert sink == [
            {
                "op": "set_tier",
                "mode": "manual",
                "mcs_index": 5,
                "fec_k": 8,
                "fec_n": 10,
            }
        ]
    finally:
        server.close()
        await server.wait_closed()


@pytest.mark.asyncio
async def test_set_tier_auto_sends_mode_only(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """The auto tier request carries only the mode."""
    sock_path = str(tmp_path / "wfb-cmd.sock")
    monkeypatch.setattr(cmd_client, "WFB_CMD_SOCK", sock_path)
    sink: list[dict] = []
    server = await _capture_one_request(sock_path, {"ok": True}, sink)
    try:
        await cmd_client.set_tier_auto()
        assert sink == [{"op": "set_tier", "mode": "auto"}]
    finally:
        server.close()
        await server.wait_closed()
