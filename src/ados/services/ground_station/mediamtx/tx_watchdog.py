"""Process-liveness watchdog loop for the ground-side ingest sidecar.

Reaps and restarts the ffmpeg subprocess when it exits unexpectedly.
The counter-delta stall path is currently disabled — see the comment
inside ``monitor_ffmpeg`` for the reasons. mediamtx's own broken-pipe
handling covers the downstream-write-stuck case until a kernel- or
parser-level liveness signal that doesn't false-positive on steady-
state RTSP push lands.
"""

from __future__ import annotations

import asyncio
import json
from typing import TYPE_CHECKING

from .ffmpeg_monitor import FFMPEG_MONITOR_TICK_SECONDS

if TYPE_CHECKING:
    import structlog

    from .manager import MediamtxGsManager


def _read_wfb_stats() -> dict | None:
    """Read the shared /run/ados/wfb-stats.json snapshot.

    The ground-side wfb_rx manager writes this file ~1 Hz. Returns the
    parsed dict when readable, or ``None`` when the file is missing /
    malformed.
    """
    from ados.core.paths import WFB_STATS_JSON

    try:
        with open(WFB_STATS_JSON) as f:
            payload = json.load(f)
    except (FileNotFoundError, OSError, ValueError):
        return None
    if not isinstance(payload, dict):
        return None
    return payload


def _wfb_packets_received() -> int | None:
    """Read packets_received from the shared wfb-stats snapshot.

    Returns the cumulative packets_received counter when readable, or
    ``None`` when the file is missing / malformed. Used by the ffmpeg
    watchdog to gate restarts so we don't loop ffmpeg every 5 s on a
    cold boot where the drone hasn't paired yet (ffmpeg's SDP probe
    gives up after 20 s with no packets and the supervisor immediately
    respawns it into the same empty-input death).
    """
    payload = _read_wfb_stats()
    if payload is None:
        return None
    value = payload.get("packets_received")
    if isinstance(value, int) and value >= 0:
        return value
    return None


def _wfb_acquire_state() -> str:
    """Channel-acquisition state from the shared wfb-stats snapshot.

    Returns one of ``idle`` / ``searching`` / ``locked`` / ``no-peer``,
    defaulting to ``idle`` when the field is absent (older agent, or the
    file not yet written). Lets the ffmpeg gate emit an actionable status
    distinguishing "the receiver is hunting for the right channel" from
    "the peer is genuinely silent", instead of an indefinite blind hold.
    """
    payload = _read_wfb_stats()
    if payload is None:
        return "idle"
    state = payload.get("acquire_state")
    if isinstance(state, str):
        return state
    return "idle"


async def monitor_ffmpeg(
    manager: MediamtxGsManager,
    shutdown: asyncio.Event,
    slog: structlog.BoundLogger,
) -> None:
    """Supervise the ffmpeg ingest until ``shutdown`` is set.

    The first attempt at boot can exit because wfb_rx hasn't received
    any radio frames yet (UDP 5600 silent, ffmpeg's probe gives up).
    Without this loop, mediamtx ends up with no publisher and the
    ground-station path stays empty forever even after pairing
    completes and the radio starts delivering.
    """
    backoff = 5.0
    max_backoff = 60.0
    while not shutdown.is_set():
        try:
            await asyncio.wait_for(
                shutdown.wait(), timeout=FFMPEG_MONITOR_TICK_SECONDS
            )
            return
        except TimeoutError:
            pass
        if not manager.ffmpeg_alive():
            # Mediamtx-core liveness FIRST: this loop only ever supervised the
            # ffmpeg ingest sidecar, never the mediamtx core that owns the RTSP
            # port. If the core crashed/OOMed, ffmpeg's push socket breaks and
            # restart_ffmpeg() can never re-bind (nothing rebinds 8554), so the
            # loop respawns ffmpeg forever and the ground path stays dark. Bring
            # the core back before the ffmpeg restart so the publisher has a
            # port to push to.
            if not manager._core.is_running():
                slog.warning("ground_mediamtx_core_dead_restarting")
                await manager._core.stop()
                if not await manager._core.start():
                    slog.error(
                        "ground_mediamtx_core_restart_failed",
                        backoff_seconds=backoff,
                    )
                    backoff = min(backoff * 2, max_backoff)
                    continue
                slog.info("ground_mediamtx_core_restarted")
            # Cold-boot gate: ffmpeg's SDP probe exits with "Output
            # file does not contain any stream" the moment its probe
            # window ends with zero inbound packets. If wfb_rx hasn't
            # received any radio frames yet there's nothing to demux,
            # so respawning ffmpeg just lights up the same 20 s
            # probe-and-die cycle. Hold off until packets are
            # actually flowing.
            received = _wfb_packets_received()
            if received is not None and received == 0:
                # Surface what the receiver is doing instead of a blind
                # hold. The wfb_rx manager sweeps the band for the
                # channel the transmitter is actually on; this gate
                # holds ffmpeg until valid packets flow, then starts it
                # the moment they do (the next tick sees received > 0).
                acquire_state = _wfb_acquire_state()
                slog.info(
                    "ground_ffmpeg_waiting_for_radio_packets",
                    acquire_state=acquire_state,
                    msg=(
                        "no valid packets yet; receiver is "
                        f"{acquire_state}. Holding ffmpeg until the link "
                        "delivers its first frame"
                    ),
                )
                continue
            slog.warning(
                "ground_ffmpeg_dead_restarting", backoff_seconds=backoff
            )
            ok = await manager.restart_ffmpeg()
            if ok:
                slog.info("ground_ffmpeg_restarted")
                backoff = 5.0
            else:
                # Capped exponential backoff so a persistently broken
                # ffmpeg doesn't spin the supervisor.
                backoff = min(backoff * 2, max_backoff)
            continue
        # NB: the in-process frame-stall watchdog is DISABLED here.
        # Both of its liveness signals false-positive on a healthy
        # ffmpeg under steady-state RTSP push on this rig:
        #
        # 1. /proc/<pid>/io wchar bumps once on the RTSP handshake
        #    burst, then barely advances for the per-frame TCP
        #    writes that follow (Linux's io accounting does not
        #    consistently count small recurring write() calls to
        #    sockets the way it counts file writes).
        # 2. The stderr `frame=NNNN` parser sees nothing for many
        #    seconds because ffmpeg block-buffers its stderr when
        #    the stream is a subprocess pipe; the 8 s stall window
        #    expires before the buffer flushes for the first time.
        #
        # Result: the watchdog reaped ffmpeg every ~10 s, mediamtx
        # never accumulated an HLS segment ring, and every segment
        # request 404'd against a freshly-rebuilt muxer. The user-
        # visible symptom was "video freezes after a few seconds"
        # on both HLS and the cascaded WebRTC fallback.
        #
        # Until a real liveness signal lands (line-buffered stderr
        # via stdbuf or -progress -, or a mediamtx-side bytesIn
        # delta probe), rely on the dead-process branch above plus
        # mediamtx's own broken-pipe recovery. ffmpeg restarts on
        # an actual crash or pipe break; that's enough.
        backoff = 5.0


__all__ = ["monitor_ffmpeg"]
