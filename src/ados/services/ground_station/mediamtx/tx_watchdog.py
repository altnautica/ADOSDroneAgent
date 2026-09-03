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
import time
from typing import TYPE_CHECKING

from .ffmpeg_monitor import (
    FFMPEG_MONITOR_TICK_SECONDS,
    FFMPEG_OUTPUT_STALL_SECONDS,
)

if TYPE_CHECKING:
    import structlog

    from .manager import MediamtxGsManager

# Freshness ceiling for the wfb-stats snapshot when it is used as a
# source-liveness gate. Matches the 10 s mtime ceiling the status route
# uses to flip a stale snapshot; past this the receiver's last write is
# too old to trust as "a source is delivering right now".
WFB_STATS_FRESH_SECONDS = 10.0

# How long ffmpeg may stay alive with no live source AND no publisher on
# /main before the monitor reaps it. Long enough that a source appearing
# mid-probe (packets start flowing, the signal flips to "live") cancels
# the reap well before it fires; short enough that an idle appliance
# stops spinning ffmpeg promptly.
NO_SOURCE_REAP_SECONDS = 15.0

# Window over which a flat mediamtx `bytesReceived` on /main, while the radio
# is confirmed delivering, means the publish has wedged.
#
# Deliberately WIDER than the ffmpeg-side output window: this counter is
# sampled once per monitor tick (2 s) rather than read from a 1 Hz progress
# block, so it has coarser resolution and less headroom against a single
# missed sample. 12 s is six ticks — a flat counter across six consecutive
# samples on a live link is not sampling noise.
INBOUND_STALL_SECONDS = 12.0


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


def _wfb_stats_age_seconds() -> float | None:
    """Wall-clock age of the wfb-stats snapshot, or ``None`` if unreadable.

    Split out from :func:`wfb_source_signal` so tests can drive the
    freshness dimension independently of the snapshot body.
    """
    from ados.core.paths import WFB_STATS_JSON

    try:
        return time.time() - WFB_STATS_JSON.stat().st_mtime
    except OSError:
        return None


def wfb_source_signal() -> str:
    """Classify whether a live radio video source is delivering packets.

    The ground ingest reads RTP off UDP 5600, fed by ``wfb_rx``. With no
    drone paired / no frames arriving, ffmpeg spawned against that silent
    port blocks in its codec probe forever (never exits, never publishes)
    and spins CPU. This gate lets the ingest lifecycle avoid that idle
    spin: only bring ffmpeg up once a source is actually present.

    Returns one of:

    * ``"live"`` — fresh snapshot reporting ``packets_received > 0``.
    * ``"silent"`` — fresh snapshot reporting ``packets_received == 0``
      (the receiver is up but no frames are flowing).
    * ``"unknown"`` — snapshot missing, stale, or malformed; the caller
      cannot conclude there is no source, so it should not defer on this
      alone.
    """
    age = _wfb_stats_age_seconds()
    if age is None or age > WFB_STATS_FRESH_SECONDS:
        return "unknown"
    received = _wfb_packets_received()
    if received is None:
        return "unknown"
    return "live" if received > 0 else "silent"


async def monitor_ffmpeg(
    manager: MediamtxGsManager,
    shutdown: asyncio.Event,
    slog: structlog.BoundLogger,
) -> None:
    """Supervise the mediamtx core AND the ffmpeg ingest until ``shutdown``.

    The first attempt at boot can exit because wfb_rx hasn't received
    any radio frames yet (UDP 5600 silent, ffmpeg's probe gives up).
    Without this loop, mediamtx ends up with no publisher and the
    ground-station path stays empty forever even after pairing
    completes and the radio starts delivering.

    Tick order matters and is the fix for two ways the ground station used to
    go dark forever:

    1. **Core liveness first, unconditionally.** The core probe used to live
       INSIDE the ``if not ffmpeg_alive()`` branch. So mediamtx OOMing while
       ffmpeg stayed blocked on a half-open RTSP socket left nothing bound to
       8554 and nothing restarting it — ffmpeg looked alive, the branch never
       ran, and the operator's video was gone permanently. The core owns the
       port every other participant needs, so it is checked every tick before
       anything else.
    2. **Then the output-counter delta.** ``ffmpeg_alive()`` (``returncode is
       None``) was the ONLY liveness test, because the previous stall watchdog
       false-positived and was disabled. A wedged-but-alive ingest therefore
       held ``/main`` and froze the video indefinitely. Process liveness is
       never proof of work: the ingest must be shown to be moving bytes, which
       is what ``ffmpeg_output_stalled`` (ffmpeg's own ``total_size``) and the
       mediamtx-side ``bytesReceived`` delta each independently prove.
    """
    backoff = 5.0
    max_backoff = 60.0
    # Monotonic timestamp of when ffmpeg was first seen alive with no
    # live source and no publisher (a stuck codec probe). Reset whenever
    # a live source or an actual publisher reappears.
    no_source_since: float | None = None
    # mediamtx-side inbound-byte watchdog state: the last counter value and
    # when it last advanced. Independent of ffmpeg's own view, and measured on
    # the far side of the RTSP socket.
    inbound_bytes: int = -1
    inbound_advanced_at: float = time.monotonic()
    while not shutdown.is_set():
        try:
            await asyncio.wait_for(
                shutdown.wait(), timeout=FFMPEG_MONITOR_TICK_SECONDS
            )
            return
        except TimeoutError:
            pass

        # --- 1. Core liveness, every tick, regardless of ffmpeg's state ---
        #
        # Hoisted out of the dead-ffmpeg branch. A dead core is unrecoverable
        # by any amount of ffmpeg respawning: ffmpeg's push socket is broken
        # and nothing else rebinds 8554.
        if not manager.core_alive():
            slog.warning("ground_mediamtx_core_dead_restarting")
            if not await manager.restart_core():
                slog.error(
                    "ground_mediamtx_core_restart_failed",
                    backoff_seconds=backoff,
                )
                backoff = min(backoff * 2, max_backoff)
                continue
            slog.info("ground_mediamtx_core_restarted")
            # The core just came back, so whatever ffmpeg was doing it was
            # pushing into a dead port. Reap it and let the branch below bring
            # a fresh one up against the new listener.
            await manager.stop_ffmpeg_ingest()
            no_source_since = None
            inbound_bytes = -1
            inbound_advanced_at = time.monotonic()

        if not manager.ffmpeg_alive():
            # ffmpeg is not alive — it cannot be a stuck probe, so clear
            # the reap timer. A fresh spawn starts its grace window clean.
            no_source_since = None
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
                inbound_bytes = -1
                inbound_advanced_at = time.monotonic()
            else:
                # Capped exponential backoff so a persistently broken
                # ffmpeg doesn't spin the supervisor.
                backoff = min(backoff * 2, max_backoff)
            continue

        # --- 2. The delta-counter checks on a LIVE ffmpeg -------------------
        #
        # Both are gated on a source actually delivering. Neither counter can
        # advance when the radio is silent, so treating "flat" as a fault
        # while there is nothing to carry would reap a healthy idle ingest —
        # which is the false-positive class that got the previous watchdog
        # disabled. When there is no source the no-source reaper below is the
        # correct owner instead.
        source_live = wfb_source_signal() == "live"

        if source_live:
            # 2a. ffmpeg's own output-side counter.
            if manager.ffmpeg_output_stalled():
                slog.warning(
                    "ground_ffmpeg_output_stalled_restarting",
                    stall_window_s=FFMPEG_OUTPUT_STALL_SECONDS,
                    total_size=manager.ffmpeg_output_bytes(),
                    msg=(
                        "ffmpeg is alive but its cumulative output-byte "
                        "counter has gone flat while the radio is "
                        "delivering; the ingest is wedged and holding /main"
                    ),
                )
                await manager.stop_ffmpeg_ingest()
                inbound_bytes = -1
                inbound_advanced_at = time.monotonic()
                continue

            # 2b. mediamtx's own view, on the far side of the RTSP socket.
            # An independent confirmation: ffmpeg can believe it is writing
            # into a socket mediamtx is no longer draining.
            current = await manager.path_inbound_bytes()
            now = time.monotonic()
            if current is None:
                # Unreachable API or absent path. Not treated as a stall —
                # it is not evidence about the byte flow, and the core probe
                # at the top of the tick is what owns "is mediamtx there".
                inbound_bytes = -1
                inbound_advanced_at = now
            elif inbound_bytes < 0 or current > inbound_bytes:
                inbound_bytes = current
                inbound_advanced_at = now
            elif (now - inbound_advanced_at) >= INBOUND_STALL_SECONDS:
                slog.warning(
                    "ground_mediamtx_inbound_stalled_restarting",
                    stall_window_s=INBOUND_STALL_SECONDS,
                    bytes_received=current,
                    msg=(
                        "mediamtx reports no new bytes on /main while the "
                        "radio is delivering; the publish has wedged"
                    ),
                )
                await manager.stop_ffmpeg_ingest()
                inbound_bytes = -1
                inbound_advanced_at = now
                continue

        # No-source reaper: an ffmpeg spawned against a silent UDP port
        # (no drone, no frames) never finishes its codec probe — it
        # neither exits nor registers a publisher, and spins CPU on an
        # idle appliance. When there is no live source AND mediamtx
        # reports no publisher on /main, that ffmpeg is a stuck probe:
        # reap it after a short grace so the idle GS runs no ffmpeg. The
        # dead-process branch above then holds it off until packets flow.
        # Requiring "no publisher" means a healthy publisher is never
        # reaped, so a brief source dropout on a live link rides through
        # untouched (its RTSP session keeps the publisher registered).
        if not source_live and not await manager.path_has_publisher():
            now = time.monotonic()
            if no_source_since is None:
                no_source_since = now
            elif (now - no_source_since) >= NO_SOURCE_REAP_SECONDS:
                slog.info(
                    "ground_ffmpeg_reaped_no_source",
                    grace_seconds=NO_SOURCE_REAP_SECONDS,
                    msg=(
                        "no live radio source and no publisher on /main; "
                        "stopping the idle ffmpeg probe. It restarts within "
                        "one tick of the first packet."
                    ),
                )
                await manager.stop_ffmpeg_ingest()
                no_source_since = None
        else:
            no_source_since = None
        backoff = 5.0


__all__ = ["monitor_ffmpeg", "wfb_source_signal"]
