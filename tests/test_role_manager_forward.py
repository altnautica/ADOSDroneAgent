"""The factory reset's role change is executed by the supervisor, not in-process.

`apply_role` forwards `set_role` to the supervisor control socket. The factory
reset wipes pair and mesh state only after it returns, so any path where no
transition ran must raise rather than hand back a result.
"""

from __future__ import annotations

import asyncio
import json
import tempfile
from pathlib import Path

import pytest

from ados.services.ground_station import role_manager as rm


async def _serve_once(sock: Path, reply: dict | None, seen: list[dict]) -> asyncio.AbstractServer:
    async def _handle(reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        seen.append(json.loads(await reader.readline()))
        if reply is not None:
            writer.write((json.dumps(reply) + "\n").encode())
            await writer.drain()
        writer.close()

    return await asyncio.start_unix_server(_handle, path=str(sock))


@pytest.fixture
def sock(monkeypatch) -> Path:
    # A short path: a Unix socket path is capped near 100 bytes.
    path = Path(tempfile.mkdtemp(dir="/tmp")) / "sup.sock"
    monkeypatch.setattr(rm, "SUPERVISOR_SOCK", str(path))
    return path


@pytest.mark.asyncio
async def test_the_supervisor_result_is_returned(sock: Path) -> None:
    seen: list[dict] = []
    server = await _serve_once(
        sock,
        {
            "ok": True,
            "role": "direct",
            "previous": "relay",
            "units_started": [],
            "units_stopped": ["ados-wfb-relay.service"],
            "ts_ms": 5,
            "noop": False,
        },
        seen,
    )
    async with server:
        result = await rm.apply_role("direct", reason="factory_reset")
    assert seen == [{"op": "set_role", "role": "direct", "reason": "factory_reset"}]
    assert result["previous"] == "relay"
    assert "ok" not in result


@pytest.mark.asyncio
async def test_no_transition_is_never_reported_as_one(sock: Path) -> None:
    with pytest.raises(rm.RoleUnavailableError):
        await rm.apply_role("direct")

    server = await _serve_once(sock, None, [])
    async with server:
        with pytest.raises(rm.RoleUnavailableError):
            await rm.apply_role("direct")

    server = await _serve_once(sock, {"ok": False, "error": "E_BIND_IN_PROGRESS"}, [])
    async with server:
        with pytest.raises(RuntimeError, match="E_BIND_IN_PROGRESS"):
            await rm.apply_role("relay")

    with pytest.raises(ValueError):
        await rm.apply_role("bogus")
