"""TX-liveness watchdog loop for the ground-side ingest sidecar.

Implements the counter-delta liveness contract: process-liveness alone
is never proof of work. The supervisor here drives a manager API that
exposes ``ffmpeg_alive()`` (process-side check) and
``ffmpeg_frame_stalled()`` (counter-delta check). When either trips,
the manager's ``restart_ffmpeg()`` is called and a backoff is applied
so a persistently broken sidecar doesn't spin the supervisor.
"""

from __future__ import annotations

import asyncio
import json
from typing import TYPE_CHECKING

from .ffmpeg_monitor import (
    FFMPEG_FRAME_STALL_SECONDS,
    FFMPEG_MONITOR_TICK_SECONDS,
)

if TYPE_CHECKING:
    import structlog

    from .manager import MediamtxGsManager


def _wfb_packets_received() -> int | None:
    """Read packets_received from the shared /run/ados/wfb-stats.json.

    The ground-side wfb_rx manager writes this file ~1 Hz. Returns the
    cumulative packets_received counter when readable, or ``None`` when
    the file is missing / malformed. Used by the ffmpeg watchdog to
    gate restarts so we don't loop ffmpeg every 5 s on a cold boot
    where the drone hasn't paired yet (ffmpeg's SDP probe gives up
    after 20 s with no packets and the supervisor immediately respawns
    it into the same empty-input death).
    """
    from ados.core.paths import WFB_STATS_JSON

    try:
        with open(WFB_STATS_JSON) as f:
            payload = json.load(f)
    except (FileNotFoundError, OSError, ValueError):
        return None
    if not isinstance(payload, dict):
        return None
    value = payload.get("packets_received")
    if isinstance(value, int) and value >= 0:
        return value
    return None


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
            # Cold-boot gate: ffmpeg's SDP probe exits with "Output
            # file does not contain any stream" the moment its probe
            # window ends with zero inbound packets. If wfb_rx hasn't
            # received any radio frames yet there's nothing to demux,
            # so respawning ffmpeg just lights up the same 20 s
            # probe-and-die cycle. Hold off until packets are
            # actually flowing.
            received = _wfb_packets_received()
            if received is not None and received == 0:
                slog.info(
                    "ground_ffmpeg_waiting_for_radio_packets",
                    msg=(
                        "wfb_rx packets_received=0; holding ffmpeg "
                        "until the link delivers its first frame"
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
        # ffmpeg is alive but may have a stuck downstream write.
        # Catch the back-pressure stall before mediamtx's RTSP
        # write-side breaks the pipe; a clean recycle here costs
        # ~1 s of viewer freeze vs the ~15-20 s freeze the broken-
        # pipe -> 5 s backoff -> codec re-probe path produces.
        if manager.ffmpeg_frame_stalled():
            slog.warning(
                "ground_ffmpeg_frame_stalled",
                last_frame=manager.ffmpeg_frame_count(),
                stall_window_s=FFMPEG_FRAME_STALL_SECONDS,
            )
            ok = await manager.restart_ffmpeg()
            if ok:
                slog.info(
                    "ground_ffmpeg_restarted_after_stall",
                )
                backoff = 5.0
            else:
                backoff = min(backoff * 2, max_backoff)
            continue
        # Healthy tick; reset the backoff so the next outage
        # restarts quickly.
        backoff = 5.0


__all__ = ["monitor_ffmpeg"]
