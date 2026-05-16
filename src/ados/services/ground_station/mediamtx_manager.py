"""Backward-compatible shim for the previous single-file mediamtx manager.

The implementation moved into the ``mediamtx`` sub-package next to
this shim. Existing imports of the form
``from ados.services.ground_station.mediamtx_manager import
MediamtxGsManager`` continue to work via the re-exports below.
"""

from __future__ import annotations

from ados.services.ground_station.mediamtx import (
    _FFMPEG_FRAME_RE,
    FFMPEG_FRAME_STALL_SECONDS,
    FFMPEG_MONITOR_TICK_SECONDS,
    GROUND_INGEST_UDP_PORT,
    GROUND_RTP_PAYLOAD_TYPE,
    GROUND_RTSP_PATH,
    GROUND_SDP_PATH,
    GROUND_WHEP_PATH,
    SPROP_PROBE_DELAY_SECONDS,
    SPROP_PROBE_DURATION_SECONDS,
    MediamtxGsManager,
    _build_sdp,
    _extract_sps_pps_from_nals,
    _parse_h264_annexb_nals,
    _probe_sprop_parameter_sets,
    _write_sdp,
    drain_ffmpeg_stderr,
    main,
    monitor_ffmpeg,
)

__all__ = [
    "MediamtxGsManager",
    "main",
    "_FFMPEG_FRAME_RE",
    "FFMPEG_FRAME_STALL_SECONDS",
    "FFMPEG_MONITOR_TICK_SECONDS",
    "drain_ffmpeg_stderr",
    "GROUND_INGEST_UDP_PORT",
    "GROUND_RTSP_PATH",
    "GROUND_WHEP_PATH",
    "GROUND_RTP_PAYLOAD_TYPE",
    "GROUND_SDP_PATH",
    "SPROP_PROBE_DELAY_SECONDS",
    "SPROP_PROBE_DURATION_SECONDS",
    "_build_sdp",
    "_write_sdp",
    "_parse_h264_annexb_nals",
    "_extract_sps_pps_from_nals",
    "_probe_sprop_parameter_sets",
    "monitor_ffmpeg",
]


if __name__ == "__main__":
    # Preserve the python -m entry point for any systemd unit that
    # invokes the legacy module path.
    import asyncio
    import sys

    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
    sys.exit(0)
