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

In lockstep (``simulation.clock_mode: lockstep``) the simulator steps past a
grant only when the agent is done with it. A plugin whose loop must keep up
opts in with ``done(t)``: every piece of its work due before sim time ``t``
is finished. The first call registers the plugin with the agent's barrier
(the participants socket beside the relay, ``DONE`` frames of 16 bytes,
little-endian ``<BBHIQ``: version, kind 3, rsv, rsv, done_ns); from then on
the simulator waits for it. A loop on ``interval(period)`` reports
``done(t + period)`` after each tick.
"""

from __future__ import annotations

import asyncio
import os
import socket
import struct
import threading
import time
from collections.abc import AsyncIterator

__all__ = [
    "CLOCK_FEED_ENV",
    "PARTICIPANTS_SOCKET",
    "STALE_AFTER_S",
    "Participant",
    "SimFeed",
    "done",
    "interval",
    "is_sim",
    "now",
    "sleep",
    "speed",
]

CLOCK_FEED_ENV = "ADOS_CLOCK_FEED"
STALE_AFTER_S = 0.5
RECONNECT_S = 1.0
_TICK = struct.Struct("<BBBBIQQd")
_DONE = struct.Struct("<BBHIQ")
PARTICIPANTS_SOCKET = "participants.sock"


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


class Participant:
    """One registration with the agent's lockstep barrier at ``path``.

    A daemon thread connects, writes the highest ``done`` reported on every
    change, and reconnects every ``RECONNECT_S`` after a refused or dropped
    connection (each connection registers afresh with the latest report)."""

    def __init__(self, path: str) -> None:
        self._path = path
        self._cond = threading.Condition()
        self._done_ns: int | None = None
        self._sent: int | None = None
        threading.Thread(target=self._run, name="ados-simclock-done", daemon=True).start()

    def done_ns(self, t_ns: int) -> None:
        """Every piece of work due before sim time ``t_ns`` is finished; a
        report at or below an earlier one changes nothing."""
        with self._cond:
            if self._done_ns is None or t_ns > self._done_ns:
                self._done_ns = t_ns
                self._cond.notify()

    def reported(self) -> int | None:
        with self._cond:
            return self._done_ns

    def _run(self) -> None:
        while True:
            try:
                with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
                    s.connect(self._path)
                    with self._cond:
                        self._sent = None
                    while True:
                        with self._cond:
                            while self._done_ns is None or self._done_ns == self._sent:
                                self._cond.wait()
                            d = self._done_ns
                        s.sendall(_DONE.pack(1, 3, 0, 0, d))
                        with self._cond:
                            self._sent = d
            except OSError:
                pass
            time.sleep(RECONNECT_S)


_PARTICIPANT: Participant | None = None
_PARTICIPANT_STARTED = False


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


def done(t: float) -> None:
    """Every piece of this plugin's work due before ADOS time ``t`` (seconds)
    is finished, its commands sent. On sim time the first call registers the
    plugin with the agent's lockstep barrier, so the simulator waits for it
    from then on: keep reporting as sim time moves (a loop on
    ``interval(period)``, with ``period`` a multiple of the run's grant,
    reports ``done(t + period)`` after each tick). A no-op on wall time."""
    global _PARTICIPANT, _PARTICIPANT_STARTED
    if not _PARTICIPANT_STARTED:
        _PARTICIPANT_STARTED = True
        relay = os.environ.get(CLOCK_FEED_ENV, "").strip()
        if relay:
            _PARTICIPANT = Participant(os.path.join(os.path.dirname(relay), PARTICIPANTS_SOCKET))
    if _PARTICIPANT is not None:
        _PARTICIPANT.done_ns(round(t * 1e9))
