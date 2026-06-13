"""The standalone API service's state-IPC reader reconnects across router restarts.

A one-shot connect + read_loop died permanently the first time the router
restarted (or when the API service won the cold-start race against the router
and the socket did not exist yet), stranding the FC-status snapshot empty so
``/api/command`` 503'd forever and ``/api/telemetry`` froze. These tests pin
that the reconnecting reader recovers the snapshot after a server restart and
after a cold start where the socket appears late.

Teardown note: ``StateIPCServer.stop()`` blocks while a client is still actively
connected, so every test stops the reader (shutdown + cancel + disconnect)
BEFORE stopping a server.
"""

from __future__ import annotations

import asyncio
import tempfile
from pathlib import Path

import pytest

from ados.core.ipc import StateIPCClient, StateIPCServer
from ados.services.api.__main__ import _state_ipc_reader


class _Log:
    """A no-op structlog-shaped sink so the reader can log without a real logger."""

    def debug(self, *a, **k) -> None: ...
    def warning(self, *a, **k) -> None: ...
    def info(self, *a, **k) -> None: ...


@pytest.fixture
def tmp_sock_dir(monkeypatch):
    with tempfile.TemporaryDirectory() as d:
        monkeypatch.setenv("ADOS_RUN_DIR", d)
        yield Path(d)


async def _wait_for(predicate, timeout: float = 5.0) -> None:
    deadline = asyncio.get_event_loop().time() + timeout
    while asyncio.get_event_loop().time() < deadline:
        if predicate():
            return
        await asyncio.sleep(0.02)
    raise AssertionError("condition not met within timeout")


async def _stop_reader(
    reader_task: asyncio.Task, client: StateIPCClient, shutdown: asyncio.Event
) -> None:
    """Tear the reader down so a connected client never deadlocks a server stop."""
    shutdown.set()
    reader_task.cancel()
    await asyncio.gather(reader_task, return_exceptions=True)
    await client.disconnect()


@pytest.mark.asyncio
async def test_reader_recovers_after_connection_drops(tmp_sock_dir):
    """The reader's connection drops (the router restarts / the socket peer goes
    away) so its read_loop returns; the reconnecting reader then re-connects to
    the live server and re-receives the snapshot. A one-shot reader would stay
    permanently disconnected after the first read_loop return — the regression."""
    sock = tmp_sock_dir / "state.sock"
    received: dict = {}
    client = StateIPCClient(sock_path=sock)
    client.set_state_handler(lambda d: received.update(d))

    shutdown = asyncio.Event()

    server = StateIPCServer(sock_path=sock)
    await server.start()
    server.publish({"fc_connected": True, "mav_type": 2})

    reader_task = asyncio.create_task(_state_ipc_reader(client, shutdown, _Log()))
    try:
        await _wait_for(lambda: received.get("fc_connected") is True)
        assert received["mav_type"] == 2

        # Force the reader's connection to drop, exactly as a router restart or
        # a transient socket error does: close the client's transport so the
        # read_loop hits EOF and returns. The reconnecting loop must come back.
        assert client.connected is True
        writer = client._writer  # the live transport for this connection
        assert writer is not None
        writer.close()
        await _wait_for(lambda: client.connected is False)

        # The same live server now publishes a fresh snapshot. The reconnecting
        # reader must re-establish and pick it up.
        await _wait_for(lambda: client.connected is True)
        server.publish({"fc_connected": True, "mav_type": 1})
        await _wait_for(lambda: received.get("mav_type") == 1)
    finally:
        await _stop_reader(reader_task, client, shutdown)
        # The reader's client is disconnected before the server stops, so the
        # server's wait_closed() is not held open by a live handler.
        await server.stop()


@pytest.mark.asyncio
async def test_reader_connects_when_socket_appears_late(tmp_sock_dir):
    """Cold-start race: the reader starts BEFORE the server exists and still
    connects once the socket appears, rather than giving up."""
    sock = tmp_sock_dir / "state.sock"
    received: dict = {}
    client = StateIPCClient(sock_path=sock)
    client.set_state_handler(lambda d: received.update(d))

    shutdown = asyncio.Event()
    reader_task = asyncio.create_task(_state_ipc_reader(client, shutdown, _Log()))
    server = StateIPCServer(sock_path=sock)
    try:
        # No server yet — the first connect attempt fails; the loop retries.
        await asyncio.sleep(0.1)
        assert client.connected is False

        await server.start()
        server.publish({"fc_connected": True, "mav_type": 10})
        await _wait_for(lambda: received.get("fc_connected") is True)
        assert received["mav_type"] == 10
    finally:
        await _stop_reader(reader_task, client, shutdown)
        await server.stop()


@pytest.mark.asyncio
async def test_reader_exits_promptly_on_shutdown(tmp_sock_dir):
    """The reader returns when shutdown is set even while connected, so service
    teardown is clean."""
    sock = tmp_sock_dir / "state.sock"
    client = StateIPCClient(sock_path=sock)
    shutdown = asyncio.Event()

    server = StateIPCServer(sock_path=sock)
    await server.start()
    server.publish({"fc_connected": True})
    reader_task = asyncio.create_task(_state_ipc_reader(client, shutdown, _Log()))
    try:
        await _wait_for(lambda: client.connected is True)
        shutdown.set()
        await client.disconnect()
        await asyncio.wait_for(reader_task, timeout=3.0)
        assert reader_task.done()
    finally:
        if not reader_task.done():
            reader_task.cancel()
            await asyncio.gather(reader_task, return_exceptions=True)
        await server.stop()
