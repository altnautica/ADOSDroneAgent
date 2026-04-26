"""Tests for the IPC layer (MAVLink + State sockets).

Covers the fast-producer / slow-consumer scenario that drove the per-client
queue + writer task refactor: producer must never block, slow clients get
disconnected before they balloon memory, and well-behaved clients keep
receiving every frame.
"""

from __future__ import annotations

import asyncio
import json
import struct
import tempfile
from pathlib import Path

import pytest

from ados.core.ipc import (
    HEADER_SIZE,
    MavlinkIPCClient,
    MavlinkIPCServer,
    StateIPCClient,
    StateIPCServer,
)


@pytest.fixture
def tmp_sock_dir(monkeypatch):
    """Redirect IPC sockets into a temp dir so tests don't touch /run."""
    with tempfile.TemporaryDirectory() as d:
        monkeypatch.setenv("ADOS_RUN_DIR", d)
        # Force re-import-time path constants by overriding via class arg
        yield Path(d)


async def _read_frame(reader: asyncio.StreamReader) -> bytes:
    header = await reader.readexactly(HEADER_SIZE)
    (length,) = struct.unpack("!I", header)
    return await reader.readexactly(length)


@pytest.mark.asyncio
async def test_mavlink_broadcast_fast_consumer_receives_all(tmp_sock_dir):
    """One healthy client should receive every broadcast frame."""
    sock = tmp_sock_dir / "mavlink.sock"
    server = MavlinkIPCServer(sock_path=sock)
    await server.start()
    try:
        reader, writer = await asyncio.open_unix_connection(str(sock))
        # Give the server a tick to register the client
        await asyncio.sleep(0.05)

        N = 200
        for i in range(N):
            server.broadcast(f"frame-{i}".encode())
        # Drain
        received: list[bytes] = []
        for _ in range(N):
            received.append(await asyncio.wait_for(_read_frame(reader), timeout=2.0))
        assert received == [f"frame-{i}".encode() for i in range(N)]

        writer.close()
        await asyncio.sleep(0.05)
    finally:
        await server.stop()


@pytest.mark.asyncio
async def test_mavlink_broadcast_slow_consumer_disconnected(tmp_sock_dir):
    """A consumer that never reads should be disconnected, not buffered forever."""
    sock = tmp_sock_dir / "mavlink.sock"
    # Tiny queue so we hit the limit fast
    server = MavlinkIPCServer(sock_path=sock, queue_depth=4)
    await server.start()
    try:
        # Open connection but never read
        _reader, _writer = await asyncio.open_unix_connection(str(sock))
        await asyncio.sleep(0.05)
        assert server.client_count == 1

        # Flood. With queue_depth=4 and a non-reading peer, the kernel buffer
        # plus the queue saturates within a few hundred frames; the server
        # must drop the slow client rather than allow unbounded growth.
        for i in range(2000):
            server.broadcast(f"big-frame-{i}".encode() * 32)
            if i % 64 == 0:
                # Yield to let the writer task try to push (it will block on
                # drain) and let the broadcast see the queue fill up.
                await asyncio.sleep(0)

        # Allow the close tasks to settle
        for _ in range(20):
            await asyncio.sleep(0.05)
            if server.client_count == 0:
                break

        assert server.client_count == 0, (
            "slow client should have been disconnected"
        )
        _writer.close()
    finally:
        await server.stop()


@pytest.mark.asyncio
async def test_mavlink_broadcast_with_no_clients_is_noop(tmp_sock_dir):
    """Broadcasting before any client connects must not raise."""
    sock = tmp_sock_dir / "mavlink.sock"
    server = MavlinkIPCServer(sock_path=sock)
    await server.start()
    try:
        for i in range(10):
            server.broadcast(b"x")
        assert server.client_count == 0
    finally:
        await server.stop()


@pytest.mark.asyncio
async def test_mavlink_command_handler_called(tmp_sock_dir):
    """Frames sent from client to server reach the registered handler."""
    sock = tmp_sock_dir / "mavlink.sock"
    server = MavlinkIPCServer(sock_path=sock)
    received: list[bytes] = []
    server.set_command_handler(received.append)
    await server.start()
    try:
        client = MavlinkIPCClient(sock_path=sock)
        await client.connect(retries=5, delay=0.1)
        client.send(b"command-1")
        client.send(b"command-2")
        await asyncio.sleep(0.1)
        assert received == [b"command-1", b"command-2"]
        await client.disconnect()
    finally:
        await server.stop()


@pytest.mark.asyncio
async def test_mavlink_multi_client_fanout(tmp_sock_dir):
    """Every healthy client should see every frame."""
    sock = tmp_sock_dir / "mavlink.sock"
    server = MavlinkIPCServer(sock_path=sock)
    await server.start()
    try:
        readers: list[asyncio.StreamReader] = []
        writers: list[asyncio.StreamWriter] = []
        for _ in range(3):
            r, w = await asyncio.open_unix_connection(str(sock))
            readers.append(r)
            writers.append(w)
        await asyncio.sleep(0.1)
        assert server.client_count == 3

        N = 50
        for i in range(N):
            server.broadcast(f"m{i}".encode())
        for r in readers:
            for i in range(N):
                frame = await asyncio.wait_for(_read_frame(r), timeout=2.0)
                assert frame == f"m{i}".encode()

        for w in writers:
            w.close()
    finally:
        await server.stop()


@pytest.mark.asyncio
async def test_state_publish_fast_consumer(tmp_sock_dir):
    """State client receives the initial snapshot then every published update."""
    sock = tmp_sock_dir / "state.sock"
    server = StateIPCServer(sock_path=sock)
    await server.start()
    try:
        # Publish before any client to seed _last_state
        server.publish({"hello": "world", "n": 0})
        await asyncio.sleep(0.05)

        client = StateIPCClient(sock_path=sock)
        await client.connect(retries=5, delay=0.1)
        states: list[dict] = []
        client.set_state_handler(states.append)
        loop_task = asyncio.create_task(client.read_loop())
        # Initial snapshot arrives
        for _ in range(20):
            await asyncio.sleep(0.05)
            if states:
                break
        assert states[0]["hello"] == "world"

        for n in range(1, 6):
            server.publish({"n": n})
        for _ in range(20):
            await asyncio.sleep(0.05)
            if any(s.get("n") == 5 for s in states):
                break
        ns = [s.get("n") for s in states]
        assert 5 in ns

        await client.disconnect()
        loop_task.cancel()
        with pytest.raises((asyncio.CancelledError, BaseException)):
            await loop_task
    finally:
        await server.stop()


@pytest.mark.asyncio
async def test_state_publish_slow_consumer_disconnected(tmp_sock_dir):
    """A state consumer that never reads should be disconnected."""
    sock = tmp_sock_dir / "state.sock"
    server = StateIPCServer(sock_path=sock, queue_depth=2)
    await server.start()
    try:
        _reader, _writer = await asyncio.open_unix_connection(str(sock))
        await asyncio.sleep(0.05)
        assert server.client_count == 1

        big = {"telemetry": "x" * 4096, "i": 0}
        for i in range(2000):
            big["i"] = i
            server.publish(big)
            if i % 32 == 0:
                await asyncio.sleep(0)

        for _ in range(20):
            await asyncio.sleep(0.05)
            if server.client_count == 0:
                break
        assert server.client_count == 0
        _writer.close()
    finally:
        await server.stop()


@pytest.mark.asyncio
async def test_state_publish_round_trip(tmp_sock_dir):
    """Round-trip JSON snapshot via the client read_loop."""
    sock = tmp_sock_dir / "state.sock"
    server = StateIPCServer(sock_path=sock)
    await server.start()
    try:
        reader, writer = await asyncio.open_unix_connection(str(sock))
        await asyncio.sleep(0.05)

        snap = {"alt": 12.5, "armed": True, "name": "test"}
        server.publish(snap)

        line = await asyncio.wait_for(reader.readline(), timeout=1.0)
        assert json.loads(line.decode()) == snap

        writer.close()
    finally:
        await server.stop()
