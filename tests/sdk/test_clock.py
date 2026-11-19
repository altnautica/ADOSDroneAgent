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
