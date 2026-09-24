"""Tests for the IPC clients (MAVLink + State sockets) and the state-wire decoder.

The native router serves both sockets; the doubles in ``tests.state_ipc_utils``
speak the same wire so the clients can be exercised without it.
"""

from __future__ import annotations

import asyncio
import struct
import tempfile
from pathlib import Path

import pytest

import ados.core.ipc as ipc_mod
from ados.core.ipc import MavlinkIPCClient, StateIPCClient
from tests.state_ipc_utils import (
    MavlinkSocketServer,
    StateSocketServer,
    encode_state_frame,
    encode_state_frame_v1,
)


@pytest.fixture
def tmp_sock_dir():
    """Keep test sockets in a temp dir so tests never touch /run."""
    with tempfile.TemporaryDirectory() as d:
        yield Path(d)


async def _wait_for(predicate, timeout: float = 2.0) -> None:
    deadline = asyncio.get_running_loop().time() + timeout
    while asyncio.get_running_loop().time() < deadline:
        if predicate():
            return
        await asyncio.sleep(0.02)
    raise AssertionError("condition not met within timeout")


@pytest.mark.asyncio
async def test_mavlink_client_sends_length_prefixed_frames(tmp_sock_dir):
    """Frames sent by the client arrive whole and in order on the socket."""
    server = MavlinkSocketServer(tmp_sock_dir / "mavlink.sock")
    await server.start()
    try:
        client = MavlinkIPCClient(sock_path=tmp_sock_dir / "mavlink.sock")
        await client.connect(retries=5, delay=0.1)
        client.send(b"command-1")
        client.send(b"command-2")
        await _wait_for(lambda: len(server.received) == 2)
        assert server.received == [b"command-1", b"command-2"]
        await client.disconnect()
    finally:
        await server.stop()


@pytest.mark.asyncio
async def test_state_client_receives_the_initial_snapshot_then_updates(tmp_sock_dir):
    """The client decodes the snapshot sent on connect, then every published one."""
    sock = tmp_sock_dir / "state.sock"
    server = StateSocketServer(sock)
    await server.start()
    try:
        server.publish({"hello": "world", "n": 0})
        client = StateIPCClient(sock_path=sock)
        await client.connect(retries=5, delay=0.1)
        states: list[dict] = []
        client.set_state_handler(states.append)
        loop_task = asyncio.create_task(client.read_loop())

        await _wait_for(lambda: bool(states))
        assert states[0] == {"hello": "world", "n": 0}

        for n in range(1, 6):
            server.publish({"n": n, "armed": n % 2 == 0})
        await _wait_for(lambda: states[-1].get("n") == 5)
        assert [s["n"] for s in states] == [0, 1, 2, 3, 4, 5]
        assert client.state == {"n": 5, "armed": False}

        await client.disconnect()
        loop_task.cancel()
        await asyncio.gather(loop_task, return_exceptions=True)
    finally:
        await server.stop()


@pytest.mark.asyncio
async def test_reader_decodes_a_v1_frame_after_a_v2_frame():
    """The self-describing reader decodes a v2 frame then a stray v1 frame.

    The router only emits v2, but the reader must still consume a stray v1
    frame on the same wire, so the per-frame sniff is exercised directly
    against a hand-built mixed stream.
    """
    reader = asyncio.StreamReader()
    reader.feed_data(encode_state_frame({"wire": "v2", "n": 1}))
    reader.feed_data(encode_state_frame_v1({"wire": "v1", "n": 2}))
    reader.feed_eof()

    first = await ipc_mod._read_state_frame(reader)
    second = await ipc_mod._read_state_frame(reader)

    assert first == {"wire": "v2", "n": 1}
    assert second == {"wire": "v1", "n": 2}


@pytest.mark.asyncio
async def test_reader_drops_the_link_on_an_out_of_range_v2_length():
    """An out-of-range length leaves its body unread, so the next bytes are not
    a frame boundary: the reader must force a reconnect rather than parse the
    body as the following frames."""
    reader = asyncio.StreamReader()
    reader.feed_data(struct.pack("!I", ipc_mod.STATE_MAX_FRAME_SIZE + 1) + b"\x00" * 16)
    reader.feed_eof()
    with pytest.raises(ConnectionError):
        await ipc_mod._read_state_frame(reader)


def test_decode_state_v2_body_skips_a_version_mismatch():
    """A v2 body carrying an unexpected version decodes to None (a skipped frame)."""
    import msgpack

    good = msgpack.packb(
        {"v": ipc_mod.STATE_V2_VERSION, "s": {"n": 1}}, use_bin_type=True
    )
    bad_version = msgpack.packb(
        {"v": ipc_mod.STATE_V2_VERSION + 1, "s": {"n": 2}}, use_bin_type=True
    )
    assert ipc_mod._decode_state_v2_body(good) == {"n": 1}
    assert ipc_mod._decode_state_v2_body(bad_version) is None
