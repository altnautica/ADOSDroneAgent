"""ADOS time for a Python plugin (``simclock.v1``).

``now()`` is the plugin's one clock for control loops, timers, freshness
checks and stamps: ``time.monotonic()`` on a real node, the simulator's time
when the host runs the plugin on a node following it. The host names the
agent's relay socket in ``ADOS_CLOCK_FEED``; the relay writes every simulator
``TICK`` (32 bytes, little-endian ``<BBBBIQQd``: version, kind, flags, rsv,
seq, sim_ns, grant_ns, speed) to each follower.

On sim time a tick with an older or repeated ``seq`` (or an earlier time) is
dropped, and a feed silent for ``STALE_AFTER_S`` of wall time is stale:
``sleep`` and ``interval`` wait until it ticks again. Nothing falls back to
wall time. Use ``time.monotonic()`` directly only for what must stay on real
time (reconnect backoff, latency measurement).
"""

from __future__ import annotations

import asyncio
import os
import socket
import struct
import threading
import time
from collections.abc import AsyncIterator

__all__ = ["CLOCK_FEED_ENV", "STALE_AFTER_S", "SimFeed", "interval", "is_sim", "now", "sleep", "speed"]

CLOCK_FEED_ENV = "ADOS_CLOCK_FEED"
STALE_AFTER_S = 0.5
RECONNECT_S = 1.0
_TICK = struct.Struct("<BBBBIQQd")


def _newer(seq: int, prev: int | None) -> bool:
    return prev is None or 0 < ((seq - prev) & 0xFFFFFFFF) < 0x80000000


class SimFeed:
    """The simulator's time, read from the relay on a background thread."""

    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._seq: int | None = None
        self._sim_ns: int | None = None
        self._speed = 0.0
        self._rx = 0.0
        # (target sim ns, future, loop) of every pending sleep.
        self._waiters: list[tuple[int, asyncio.Future[None], asyncio.AbstractEventLoop]] = []

    def take(self, data: bytes, rx: float) -> bool:
        """One tick received at wall ``rx``; False when dropped."""
        if len(data) != _TICK.size:
            return False
        v, kind, _flags, _rsv, seq, sim_ns, _grant, spd = _TICK.unpack(data)
        if v != 1 or kind != 1:
            return False
        with self._lock:
            if not _newer(seq, self._seq) or (self._sim_ns is not None and sim_ns < self._sim_ns):
                return False
            self._seq, self._sim_ns, self._speed, self._rx = seq, sim_ns, spd, rx
            due = [w for w in self._waiters if w[0] <= sim_ns]
            self._waiters = [w for w in self._waiters if w[0] > sim_ns]
        for _, fut, loop in due:
            loop.call_soon_threadsafe(_resolve, fut)
        return True

    def stale(self, at: float | None = None) -> bool:
        with self._lock:
            return self._sim_ns is None or (time.monotonic() if at is None else at) - self._rx > STALE_AFTER_S

    def sim_ns(self) -> int | None:
        with self._lock:
            return self._sim_ns

    def speed(self) -> float:
        with self._lock:
            return self._speed

    async def wait_until_ns(self, target: int) -> None:
        loop = asyncio.get_running_loop()
        fut: asyncio.Future[None] = loop.create_future()
        with self._lock:
            if self._sim_ns is not None and self._sim_ns >= target:
                return
            self._waiters.append((target, fut, loop))
        await fut

    def follow(self, path: str) -> None:
        """Read the relay at ``path`` forever on a daemon thread."""
        threading.Thread(target=self._run, args=(path,), name="ados-simclock", daemon=True).start()

    def _run(self, path: str) -> None:
        while True:
            try:
                with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
                    s.connect(path)
                    buf = b""
                    while True:
                        chunk = s.recv(4096)
                        if not chunk:
                            break
                        buf += chunk
                        while len(buf) >= _TICK.size:
                            self.take(buf[: _TICK.size], time.monotonic())
                            buf = buf[_TICK.size:]
            except OSError:
                pass
            time.sleep(RECONNECT_S)


def _resolve(fut: asyncio.Future[None]) -> None:
    if not fut.done():
        fut.set_result(None)


_FEED: SimFeed | None = None
_STARTED = False


def _feed() -> SimFeed | None:
    global _FEED, _STARTED
    if not _STARTED:
        _STARTED = True
        path = os.environ.get(CLOCK_FEED_ENV, "").strip()
        if path:
            _FEED = SimFeed()
            _FEED.follow(path)
    return _FEED


def is_sim() -> bool:
    """The plugin runs on the simulator's clock."""
    return _feed() is not None


def now() -> float:
    """ADOS time in seconds: monotonic on wall, sim seconds on sim (0 before
    the first tick)."""
    f = _feed()
    if f is None:
        return time.monotonic()
    return (f.sim_ns() or 0) / 1e9


def speed() -> float | None:
    """The simulator's pacing speed; None on wall time."""
    f = _feed()
    return None if f is None else f.speed()


async def sleep(seconds: float) -> None:
    """Wait ``seconds`` of ADOS time. On sim it waits for the feed, so a stale
    feed holds it."""
    f = _feed()
    if f is None:
        await asyncio.sleep(seconds)
        return
    await f.wait_until_ns((f.sim_ns() or 0) + int(seconds * 1e9))


async def interval(period: float) -> AsyncIterator[float]:
    """Tick every ``period`` of ADOS time (missed ticks skipped); yields now()."""
    nxt = now()
    while True:
        t = now()
        if t < nxt:
            await sleep(nxt - t)
            t = now()
        nxt += period * (int((t - nxt) / period) + 1)
        yield t
