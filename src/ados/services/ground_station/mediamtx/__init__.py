"""mediamtx lifecycle for the ground-station profile.

The implementation now lives in per-concern files alongside this barrel:

* ``manager.py`` — :class:`MediamtxGsManager` (process lifecycle,
  config generation, ffmpeg ingest spawn / restart, graceful shutdown)
  plus the ``main()`` service entry point.
* ``rtsp_config.py`` — SDP body construction and on-disk writeback,
  H.264 Annex-B NAL parsing, sprop probe coroutine, wire constants
  (UDP ingest port, RTSP path, WHEP path, payload type).
* ``ffmpeg_monitor.py`` — ffmpeg stderr drain that surfaces error
  lines and parses ``frame=NNNN`` progress tokens.
* ``tx_watchdog.py`` — TX-liveness supervisor loop that reaps and
  restarts a wedged ffmpeg sidecar before mediamtx's broken-pipe
  cascade fires.

Existing callers (``from ados.services.ground_station.mediamtx_manager
import MediamtxGsManager``) keep working unchanged via the legacy
import path that re-exports the same names.
"""

from __future__ import annotations

from .ffmpeg_monitor import (
    _FFMPEG_FRAME_RE,
    FFMPEG_FRAME_STALL_SECONDS,
    FFMPEG_MONITOR_TICK_SECONDS,
    drain_ffmpeg_stderr,
)
from .manager import MediamtxGsManager, main
from .process_argv import build_ffmpeg_ingest_argv, build_mediamtx_yaml
from .rtsp_config import (
    GROUND_INGEST_UDP_PORT,
    GROUND_RTP_PAYLOAD_TYPE,
    GROUND_RTSP_PATH,
    GROUND_SDP_PATH,
    GROUND_WHEP_PATH,
    SPROP_PROBE_DELAY_SECONDS,
    SPROP_PROBE_DURATION_SECONDS,
    _build_sdp,
    _extract_sps_pps_from_nals,
    _parse_h264_annexb_nals,
    _probe_sprop_parameter_sets,
    _write_sdp,
    bake_sprop_into_sdp,
)
from .tx_watchdog import monitor_ffmpeg, wfb_source_signal

__all__ = [
    # manager
    "MediamtxGsManager",
    "main",
    # ffmpeg monitor
    "_FFMPEG_FRAME_RE",
    "FFMPEG_FRAME_STALL_SECONDS",
    "FFMPEG_MONITOR_TICK_SECONDS",
    "drain_ffmpeg_stderr",
    # rtsp / sdp config
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
    "bake_sprop_into_sdp",
    "build_mediamtx_yaml",
    "build_ffmpeg_ingest_argv",
    # watchdog
    "monitor_ffmpeg",
    "wfb_source_signal",
]
