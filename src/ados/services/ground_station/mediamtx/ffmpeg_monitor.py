"""ffmpeg stderr parsing for the ground-side ingest sidecar.

Pulled out of ``manager.py`` so the regex + drain helper can be tested
or reused without owning the manager's instance state. The drain
coroutine is a free function taking the subprocess and a callback that
records output-byte advances; the manager wires its own counters
through that callback.

Why parse ``total_size=`` and not ``frame=``: the ingest runs ``-c:v
copy``, and the counter that matters is the one on the OUTPUT side.
``total_size`` is the running count of bytes the muxer has written to
the RTSP push, so a monotone increase is direct proof the publish is
still moving. ``frame=`` counts what the demuxer pulled in, and the
legacy single-line status report that carried it was the reason the
stall watchdog had to be disabled: ffmpeg block-buffers stderr behind a
subprocess pipe and suppresses the status line entirely when stderr is
not a tty, so the parser starved on a healthy process and the watchdog
reaped it every ~10 s.

``-progress pipe:2`` (see ``process_argv.build_ffmpeg_ingest_argv``)
is what makes this reliable: ffmpeg emits and flushes a structured
``key=value`` block once per second regardless of tty-ness. A wedged
ffmpeg keeps emitting ``progress=continue`` with a FROZEN
``total_size``, which is exactly the "alive but doing no work" state
process liveness cannot see.
"""

from __future__ import annotations

import asyncio
import re
from collections.abc import Callable

from ados.core.logging import get_logger

log = get_logger("ground_station.mediamtx")

# Match the cumulative output-byte counter in an ffmpeg `-progress` block:
# `total_size=2097152`. Non-anchored and we take the *last* match per record,
# which is the freshest value when several landed in one read.
_FFMPEG_TOTAL_SIZE_RE = re.compile(rb"total_size=\s*(\d+)")

# Window over which a static `total_size=` counter means the publish has
# wedged. The downstream symptom is mediamtx's RTSP write socket eventually
# breaking the pipe; recycling ffmpeg *before* that back-pressure becomes a
# multi-second browser outage is the whole point.
#
# 8 s is ~8 progress blocks at the 1 Hz `-progress` cadence, so a genuine
# stall is unambiguous rather than one missed flush.
FFMPEG_OUTPUT_STALL_SECONDS = 8.0

# Grace before the output-stall check may trip at all, measured from the
# spawn. ffmpeg's RTSP handshake plus the wait for the first IDR can take
# 5-10 s on a cold start, and the ingest additionally allows a 20 s
# probesize/analyzeduration window for SPS/PPS discovery — so nothing is
# expected on the output side for a while and tripping inside that window is
# a false positive by construction.
FFMPEG_FIRST_OUTPUT_GRACE_SECONDS = 28.0

# How often the monitor in main() polls liveness. Tight enough to react
# inside FFMPEG_OUTPUT_STALL_SECONDS, loose enough to stay cheap.
FFMPEG_MONITOR_TICK_SECONDS = 2.0

# Bounded read size for the stderr splitter.
_READ_CHUNK = 4096
# Ceiling on the partial-record buffer, so a stream that never produces a
# separator cannot grow it without limit.
_MAX_PARTIAL = 64 * 1024


async def drain_ffmpeg_stderr(
    proc: asyncio.subprocess.Process,
    on_output_bytes: Callable[[int], None],
) -> None:
    """Drain ``proc.stderr`` and surface error lines to the journal.

    Parses ``total_size=N`` tokens from each record and invokes
    ``on_output_bytes(latest)`` whenever a fresher value is observed.
    The caller (the manager) records the wall time in its own state so
    the stall watchdog can reason about elapsed silence.

    Records are split on ``\\n`` **or** ``\\r``. Both are needed and
    neither alone is sufficient: ``-progress pipe:2`` emits newline-
    terminated ``key=value`` lines, while ffmpeg's real diagnostics and
    its legacy in-place status report use ``\\r``. The previous
    implementation used ``readuntil(b"\\r")``, which under ``-progress``
    would find no separator at all and only yield once the 64 KB
    StreamReader limit overran — at ~200 bytes per progress block that
    is roughly five minutes of delay on the one signal the watchdog
    depends on.
    """
    if proc.stderr is None:
        return
    try:
        last_bytes = -1
        partial = b""
        while True:
            chunk = await proc.stderr.read(_READ_CHUNK)
            if not chunk:
                break
            partial += chunk
            # Normalise both separators to `\n` so one split handles the
            # `-progress` block and the `\r` diagnostics together.
            records = partial.replace(b"\r", b"\n").split(b"\n")
            # The trailing element is an incomplete record; hold it over.
            partial = records.pop()
            if len(partial) > _MAX_PARTIAL:
                # No separator in 64 KB: treat what we have as a record
                # rather than buffering without bound.
                records.append(partial)
                partial = b""
            for record in records:
                if not record:
                    continue
                # Update the counter before logging, so a chatty log path
                # can never cost a missed liveness signal.
                matches = _FFMPEG_TOTAL_SIZE_RE.findall(record)
                if matches:
                    try:
                        latest = int(matches[-1])
                    except ValueError:
                        latest = last_bytes
                    if latest > last_bytes:
                        last_bytes = latest
                        on_output_bytes(latest)
                text = record.decode(errors="replace").rstrip()
                if not text:
                    continue
                if _is_progress_line(text):
                    # The `-progress` block is ~12 lines/s of pure noise
                    # once parsed. Dropped from the log entirely.
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


def _is_progress_line(text: str) -> bool:
    """True for a routine ``-progress`` telemetry line (a lowercase key,
    then ``=``).

    Real ffmpeg diagnostics start with ``[component @ addr]``, a capital
    word, or a path, so they do not match this shape and still reach the
    log.
    """
    key, sep, _ = text.partition("=")
    if not sep or not key:
        return False
    return key.replace("_", "").isalnum() and key.islower()


__all__ = [
    "_FFMPEG_TOTAL_SIZE_RE",
    "FFMPEG_FIRST_OUTPUT_GRACE_SECONDS",
    "FFMPEG_MONITOR_TICK_SECONDS",
    "FFMPEG_OUTPUT_STALL_SECONDS",
    "drain_ffmpeg_stderr",
]
