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
from typing import TYPE_CHECKING

from .ffmpeg_monitor import (
    FFMPEG_FRAME_STALL_SECONDS,
    FFMPEG_MONITOR_TICK_SECONDS,
)

if TYPE_CHECKING:
    import structlog

    from .manager import MediamtxGsManager


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
