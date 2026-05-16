"""ffmpeg stderr parsing for the ground-side ingest sidecar.

Pulled out of ``manager.py`` so the regex + drain helper can be tested
or reused without owning the manager's instance state. The drain
coroutine is a free function taking the subprocess and a callback that
records frame-counter advances; the manager wires its own counters
through that callback.

Why parse ``frame=NNNN``: ffmpeg writes progress as carriage-return
records on stderr while it's healthy. A flat counter for several
seconds is a strong signal the publisher has wedged on the downstream
RTSP write side, well before mediamtx's broken-pipe restart cascade
fires. Catching that signal lets the manager recycle ffmpeg cleanly
(~1 s viewer freeze) instead of waiting for the cascade
(~15-20 s freeze).
"""

from __future__ import annotations

import asyncio
import re
from collections.abc import Callable

from ados.core.logging import get_logger

log = get_logger("ground_station.mediamtx")

# Match the `frame=NNNN` token in ffmpeg's stderr progress lines.
# ffmpeg emits progress as a single carriage-return-terminated record:
# `frame= 1234 fps= 30 q=-1.0 size=N/A time=... bitrate=N/A speed=1x`
# Multiple progress records can land in one readline() buffer when
# stderr is drained slowly, so the search is non-anchored and we take
# the *last* match in the line to track the freshest frame count.
_FFMPEG_FRAME_RE = re.compile(rb"frame=\s*(\d+)")

# Window over which a static `frame=` counter means the publisher has
# gone silent. The downstream symptom is mediamtx's RTSP write socket
# eventually breaking the pipe; we want to recycle ffmpeg *before* that
# back-pressure causes a multi-second outage in the browser viewer.
FFMPEG_FRAME_STALL_SECONDS = 8.0

# How often the monitor in main() polls liveness. Tight enough to react
# inside FFMPEG_FRAME_STALL_SECONDS, loose enough to stay cheap.
FFMPEG_MONITOR_TICK_SECONDS = 2.0


async def drain_ffmpeg_stderr(
    proc: asyncio.subprocess.Process,
    on_frame: Callable[[int], None],
) -> None:
    """Drain ``proc.stderr`` and surface error lines to the journal.

    Parses ``frame=NNNN`` progress tokens from each chunk and invokes
    ``on_frame(latest)`` whenever a fresher count is observed. The
    caller (the manager) records the wall time in its own state so the
    stall watchdog can reason about elapsed silence.
    """
    if proc.stderr is None:
        return
    try:
        last_count = 0
        while True:
            # readuntil() on `\r` keeps each carriage-return-
            # terminated progress record as its own line. ffmpeg
            # uses `\r` for in-place progress; readline() (which
            # stops at `\n`) would batch many records into one
            # giant string and we'd only see the freshest one
            # whenever the journal eventually flushed.
            try:
                chunk = await proc.stderr.readuntil(b"\r")
            except asyncio.IncompleteReadError as exc:
                chunk = exc.partial
                if not chunk:
                    break
            except asyncio.LimitOverrunError:
                # readuntil's default StreamReader buffer is 64 KB;
                # an absurdly long progress line shouldn't happen
                # in practice, but fall back to a bounded read so
                # we never deadlock the drain.
                chunk = await proc.stderr.read(4096)
                if not chunk:
                    break
            if not chunk:
                break
            # Parse frame counter — keep the last match per chunk,
            # which is the freshest record when multiple landed in
            # one read. Update before logging so a stalled ffmpeg
            # whose progress lines have stopped streaming doesn't
            # also cost us a missed liveness signal.
            matches = _FFMPEG_FRAME_RE.findall(chunk)
            if matches:
                try:
                    latest = int(matches[-1])
                except ValueError:
                    latest = last_count
                if latest > last_count:
                    last_count = latest
                    on_frame(latest)
            text = chunk.decode(errors="replace").rstrip()
            if not text:
                continue
            lower = text.lower()
            if (
                "error" in lower
                or "failed" in lower
                or "could not" in lower
                or "no such" in lower
            ):
                log.warning("ground_ffmpeg_stderr", line=text)
            else:
                log.debug("ground_ffmpeg_stderr", line=text)
    except (asyncio.CancelledError, Exception):
        pass


__all__ = [
    "_FFMPEG_FRAME_RE",
    "FFMPEG_FRAME_STALL_SECONDS",
    "FFMPEG_MONITOR_TICK_SECONDS",
    "drain_ffmpeg_stderr",
]
