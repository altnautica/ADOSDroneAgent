"""ADOS time in the Python SDK: the relay feed and its timers."""

from __future__ import annotations

import asyncio
import socket
import struct
import tempfile
import time
from pathlib import Path

from ados.sdk import clock

TICK = struct.Struct("<BBBBIQQd")


def tick(seq: int, ms: int) -> bytes:
    return TICK.pack(1, 1, 0, 0, seq, ms * 1_000_000, (ms + 20) * 1_000_000, 3.0)


def test_ticks_are_taken_in_order() -> None:
    f = clock.SimFeed()
    assert f.stale()
    assert f.take(tick(5, 50), 1.0)
    assert not f.take(tick(4, 60), 1.0), "older seq"
    assert not f.take(tick(5, 70), 1.0), "repeated seq"
    assert not f.take(tick(6, 40), 1.0), "time going back"
    assert f.take(tick(6, 60), 1.0)
    assert f.sim_ns() == 60_000_000
    assert f.speed() == 3.0
    assert not f.take(b"\x00" * 32, 1.0)
    assert f.stale(at=1.6) and not f.stale(at=1.2)


def test_sleep_waits_for_sim_time() -> None:
    async def run() -> None:
        f = clock.SimFeed()
        f.take(tick(1, 0), time.monotonic())
        waiter = asyncio.ensure_future(f.wait_until_ns(30_000_000))
        await asyncio.sleep(0.05)
        assert not waiter.done(), "wall time passing does not move sim time"
        f.take(tick(2, 20), time.monotonic())
        await asyncio.sleep(0.01)
        assert not waiter.done()
        f.take(tick(3, 30), time.monotonic())
        await asyncio.wait_for(waiter, 1.0)

    asyncio.run(run())


def test_the_feed_follows_a_relay_socket() -> None:
    with tempfile.TemporaryDirectory() as d:
        path = str(Path(d) / "relay.sock")
        srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        srv.bind(path)
        srv.listen(1)
        f = clock.SimFeed()
        f.follow(path)
        srv.settimeout(3)
        conn, _ = srv.accept()
        b = tick(1, 40)
        conn.sendall(b[:9])
        conn.sendall(b[9:])
        deadline = time.monotonic() + 3
        while f.sim_ns() != 40_000_000 and time.monotonic() < deadline:
            time.sleep(0.01)
        assert f.sim_ns() == 40_000_000
        conn.close()
        srv.close()


def test_without_the_env_it_is_wall(monkeypatch) -> None:
    monkeypatch.delenv(clock.CLOCK_FEED_ENV, raising=False)
    monkeypatch.setattr(clock, "_STARTED", False)
    monkeypatch.setattr(clock, "_FEED", None)
    assert not clock.is_sim()
    assert abs(clock.now() - time.monotonic()) < 0.1
    assert clock.speed() is None
    monkeypatch.setattr(clock, "_PARTICIPANT_STARTED", False)
    monkeypatch.setattr(clock, "_PARTICIPANT", None)
    clock.done(1.0)
    assert clock._PARTICIPANT is None


# The bytes the agent's `ados_protocol::simclock::wire` test reads as `Done { done_ns: 1.54 s }`.
GOLDEN_DONE = bytes(
    [0x01, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x89, 0xCA, 0x5B, 0x00, 0x00, 0x00, 0x00]
)


def _recv_exact(conn: socket.socket, n: int) -> bytes:
    buf = b""
    while len(buf) < n:
        chunk = conn.recv(n - len(buf))
        assert chunk, "the participant closed the connection"
        buf += chunk
    return buf


def test_done_registers_beside_the_relay_and_writes_the_golden_frame(monkeypatch) -> None:
    with tempfile.TemporaryDirectory() as d:
        srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        srv.bind(str(Path(d) / clock.PARTICIPANTS_SOCKET))
        srv.listen(1)
        srv.settimeout(3)
        monkeypatch.setenv(clock.CLOCK_FEED_ENV, str(Path(d) / "relay.sock"))
        monkeypatch.setattr(clock, "_PARTICIPANT_STARTED", False)
        monkeypatch.setattr(clock, "_PARTICIPANT", None)
        clock.done(1.54)
        clock.done(1.0)
        conn, _ = srv.accept()
        conn.settimeout(3)
        assert _recv_exact(conn, 16) == GOLDEN_DONE
        assert clock._PARTICIPANT.reported() == 1_540_000_000
        clock.done(1.6)
        assert _recv_exact(conn, 16)[8:] == (1_600_000_000).to_bytes(8, "little")
        conn.close()
        srv.close()
