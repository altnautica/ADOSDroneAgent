"""The state client stops reporting a snapshot the router no longer refreshes."""

from __future__ import annotations

import asyncio
import tempfile
from pathlib import Path

import pytest

import ados.core.ipc as ipc_mod
from ados.api.runtime import ApiRuntimeFacade
from ados.core.ipc import StateIPCClient
from tests.state_ipc_utils import StateSocketServer


class _Clock:
    def __init__(self) -> None:
        self.now = 1000.0

    def __call__(self) -> float:
        return self.now


async def _wait_for(predicate, timeout: float = 2.0) -> None:
    deadline = asyncio.get_running_loop().time() + timeout
    while asyncio.get_running_loop().time() < deadline:
        if predicate():
            return
        await asyncio.sleep(0.02)
    raise AssertionError("condition not met within timeout")


@pytest.mark.asyncio
async def test_snapshot_goes_stale_after_three_silent_seconds():
    clock = _Clock()
    with tempfile.TemporaryDirectory() as d:
        sock = Path(d) / "state.sock"
        server = StateSocketServer(sock)
        await server.start()
        try:
            server.publish({"fc_connected": True, "armed": True})
            client = StateIPCClient(sock_path=sock, clock=clock)
            await client.connect(retries=5, delay=0.1)
            loop_task = asyncio.create_task(client.read_loop())
            await _wait_for(lambda: bool(client.state))

            facade = ApiRuntimeFacade(type("R", (), {"state_client": client})())
            assert facade.fc_status().connected is True

            clock.now += ipc_mod.STATE_STALE_AFTER_S - 0.5
            assert client.state == {"fc_connected": True, "armed": True}

            clock.now += 1.0
            assert client.stale is True
            assert client.state == {}
            assert facade.fc_status().connected is False

            await client.disconnect()
            loop_task.cancel()
            await asyncio.gather(loop_task, return_exceptions=True)
        finally:
            await server.stop()


@pytest.mark.asyncio
async def test_snapshot_is_dropped_when_the_router_connection_ends():
    with tempfile.TemporaryDirectory() as d:
        sock = Path(d) / "state.sock"
        server = StateSocketServer(sock)
        await server.start()
        server.publish({"fc_connected": True})
        client = StateIPCClient(sock_path=sock)
        await client.connect(retries=5, delay=0.1)
        loop_task = asyncio.create_task(client.read_loop())
        await _wait_for(lambda: bool(client.state))

        await server.stop()
        await asyncio.wait_for(loop_task, timeout=2.0)

        assert client.connected is False
        assert client.state == {}
