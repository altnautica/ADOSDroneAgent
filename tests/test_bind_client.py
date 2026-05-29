"""Tests for the bind control-socket client.

``bind_client`` forwards every bind request to the supervisor's Unix
control socket and reads the cross-process liveness sentinel. The socket
is the sole producer of bind sessions, so an unreachable socket is a
hard error on ``forward_start_bind`` and a ``{}`` snapshot on
``forward_status``. These tests cover the sentinel reader, the
unreachable-socket paths, and a happy-path round-trip against a tiny
in-test Unix listener.
"""

from __future__ import annotations

import asyncio
import json
from pathlib import Path

import pytest

from ados.services.wfb import bind_client
from ados.services.wfb.bind_client import (
    BindBusyError,
    BindUnavailableError,
    forward_start_bind,
    forward_status,
    read_bind_sentinel_active,
)


@pytest.mark.parametrize(
    ("body", "expected"),
    [
        ('{"active": true}', True),
        ('{"active": false}', False),
        ('{"active": 1}', True),
        ('{"other": true}', False),
        ("not json at all", False),
        ("[1, 2, 3]", False),
        ('"just a string"', False),
    ],
)
def test_read_bind_sentinel_active_payloads(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    body: str,
    expected: bool,
) -> None:
    """The sentinel reader maps each payload shape to the right bool."""
    sentinel = tmp_path / "bind-state.json"
    sentinel.write_text(body, encoding="utf-8")
    monkeypatch.setattr(bind_client, "BIND_STATE_SENTINEL", str(sentinel))
    assert read_bind_sentinel_active() is expected


def test_read_bind_sentinel_active_absent_file(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """An absent sentinel file reads as inactive, never raises."""
    monkeypatch.setattr(
        bind_client, "BIND_STATE_SENTINEL", str(tmp_path / "does-not-exist.json")
    )
    assert read_bind_sentinel_active() is False


@pytest.mark.asyncio
async def test_forward_status_unreachable_socket_returns_empty(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """An unreachable control socket yields an empty status snapshot."""
    monkeypatch.setattr(
        bind_client, "SUPERVISOR_SOCK", str(tmp_path / "does-not-exist.sock")
    )
    assert await forward_status() == {}


@pytest.mark.asyncio
async def test_forward_start_bind_unreachable_socket_raises(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """An unreachable control socket is a hard error on start_bind."""
    monkeypatch.setattr(
        bind_client, "SUPERVISOR_SOCK", str(tmp_path / "does-not-exist.sock")
    )
    with pytest.raises(BindUnavailableError):
        await forward_start_bind(
            role="drone",
            source="operator",
            peer_device_id=None,
            cancel_event=None,
            timeout=None,
        )


async def _serve_one_reply(sock_path: str, reply: dict) -> asyncio.AbstractServer:
    """Spin up a Unix listener that answers one request per connection.

    Reads the newline-terminated request, writes ``reply`` as a single
    newline-terminated JSON line, then closes. Returns the server so the
    caller can close it in a finally block.
    """

    async def _handle(
        reader: asyncio.StreamReader, writer: asyncio.StreamWriter
    ) -> None:
        await reader.readline()
        writer.write((json.dumps(reply) + "\n").encode("utf-8"))
        await writer.drain()
        writer.close()

    return await asyncio.start_unix_server(_handle, path=sock_path)


@pytest.mark.asyncio
async def test_forward_start_bind_happy_path(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """A successful reply round-trips into the session dict."""
    sock_path = str(tmp_path / "supervisor.sock")
    monkeypatch.setattr(bind_client, "SUPERVISOR_SOCK", sock_path)
    server = await _serve_one_reply(
        sock_path, {"ok": True, "session": {"state": "paired"}}
    )
    try:
        result = await forward_start_bind(
            role="drone",
            source="operator",
            peer_device_id=None,
            cancel_event=None,
            timeout=5.0,
        )
        assert result == {"state": "paired"}
    finally:
        server.close()
        await server.wait_closed()


@pytest.mark.asyncio
async def test_forward_start_bind_busy_raises(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """An E_BIND_IN_PROGRESS reply surfaces as BindBusyError."""
    sock_path = str(tmp_path / "supervisor.sock")
    monkeypatch.setattr(bind_client, "SUPERVISOR_SOCK", sock_path)
    server = await _serve_one_reply(
        sock_path, {"ok": False, "error": "E_BIND_IN_PROGRESS"}
    )
    try:
        with pytest.raises(BindBusyError):
            await forward_start_bind(
                role="drone",
                source="operator",
                peer_device_id=None,
                cancel_event=None,
                timeout=5.0,
            )
    finally:
        server.close()
        await server.wait_closed()
