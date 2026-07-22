"""RTSP / SDP configuration helpers for the ground-side mediamtx.

Pure data + filesystem helpers extracted from the original
``mediamtx_manager.py``. Nothing here owns a process or a long-lived
asyncio task; ``manager.py`` consumes these helpers when it spins up
the mediamtx subprocess and the ffmpeg ingest sidecar.

Three concerns live in this module:

* SDP body construction and on-disk writeback
  (``_build_sdp`` + ``_write_sdp``) so ffmpeg's ``-f sdp`` ingest can
  parse the wfb_rx UDP stream without an RTSP DESCRIBE round-trip.
* H.264 Annex-B NAL parsing + SPS/PPS extraction
  (``_parse_h264_annexb_nals`` + ``_extract_sps_pps_from_nals``) used
  by the sprop bake to enrich the SDP after first IDR.
* The probe coroutine (``_probe_sprop_parameter_sets``) and the
  one-shot bake task (``bake_sprop_into_sdp``) that captures a few
  seconds of the live stream and updates the SDP in place per
  RFC 6184 §8.1.

Wire constants (UDP ingest port, RTSP path, WHEP path, payload type,
SDP file location, probe timing) live alongside.
"""

from __future__ import annotations

import asyncio
import base64
import re
import shutil
from pathlib import Path

from ados.core.logging import get_logger

log = get_logger("ground_station.mediamtx")

GROUND_INGEST_UDP_PORT = 5600
GROUND_RTSP_PATH = "main"
GROUND_RTP_PAYLOAD_TYPE = 96
# SDP describing the RTP stream wfb_rx pushes to UDP 5600. ffmpeg reads
# this file via ``-f sdp -i ...`` so it knows the codec / clock rate /
# packetization mode without an RTSP DESCRIBE round-trip (wfb_rx is a
# one-way broadcaster, no RTSP server to query). We write a fresh copy
# from generate_config() each time to track port / payload-type changes.
# Sits next to /etc/ados/wfb/{rx,tx}.key in the same writable dir.
GROUND_SDP_PATH = Path("/etc/ados/wfb/video.sdp")

# Number of seconds to wait for ffmpeg + mediamtx to be healthy before
# probing the live stream for SPS/PPS NAL units.
SPROP_PROBE_DELAY_SECONDS = 6.0

# How long to read the H.264 bitstream from mediamtx before giving up.
# Both SPS and PPS are emitted at every IDR, and the encoder's GOP is
# fps/2 (~500 ms), so a 3 s window catches at least 4 IDR boundaries —
# more than enough to find SPS + PPS even if the first ones arrive at
# a half-second offset from the probe start.
SPROP_PROBE_DURATION_SECONDS = 3.0


def _build_sdp(
    udp_port: int,
    payload_type: int,
    sprop_parameter_sets: str | None = None,
) -> str:
    """Return the SDP body that describes the wfb_rx RTP stream.

    When sprop_parameter_sets is given (e.g., "Z0LAKdoB...,aM48gA=="),
    the fmtp line carries the base64-encoded SPS+PPS pair per RFC 6184
    §8.1. That gives the downstream WebRTC reader access to the H.264
    decoder configuration out of band, so the browser decoder can
    recover from a transient reference-frame loss without depending on
    a fresh in-band parameter set arriving at the next IDR. Without
    sprop, Chrome's libwebrtc decoder will freeze on the last successful
    frame after any sync loss until the WHEP session is renegotiated.

    Stripped back to the minimal H.264 over RTP/AVP descriptor otherwise
    — ffmpeg's -f sdp ingest opens RTCP on RTP+1 by default, and the
    LCD render-tap previously claimed that port. We've moved the LCD tap
    to 5605 (see ground_station/wfb_rx.py), freeing udp_port+1 for
    ffmpeg's default RTCP socket, so no explicit a=rtcp hint is needed.
    """
    if sprop_parameter_sets:
        fmtp_line = (
            f"a=fmtp:{payload_type} packetization-mode=1;"
            f"sprop-parameter-sets={sprop_parameter_sets}\n"
        )
    else:
        fmtp_line = f"a=fmtp:{payload_type} packetization-mode=1\n"
    return (
        "v=0\n"
        "o=- 0 0 IN IP4 127.0.0.1\n"
        "s=ADOS Video\n"
        "c=IN IP4 127.0.0.1\n"
        "t=0 0\n"
        f"m=video {udp_port} RTP/AVP {payload_type}\n"
        f"a=rtpmap:{payload_type} H264/90000\n"
        f"{fmtp_line}"
    )


def _write_sdp(
    udp_port: int,
    payload_type: int = GROUND_RTP_PAYLOAD_TYPE,
    sprop_parameter_sets: str | None = None,
) -> Path:
    """Write the SDP to GROUND_SDP_PATH and return the path. Idempotent.

    If sprop_parameter_sets is None and the existing SDP on disk has a
    sprop line baked in, the existing value is preserved — callers that
    don't know the parameter sets shouldn't blow away a baked SDP.
    """
    GROUND_SDP_PATH.parent.mkdir(parents=True, exist_ok=True)
    if sprop_parameter_sets is None and GROUND_SDP_PATH.exists():
        try:
            existing = GROUND_SDP_PATH.read_text()
        except OSError:
            existing = ""
        match = re.search(
            r"sprop-parameter-sets=([^\s;]+)", existing
        )
        if match:
            sprop_parameter_sets = match.group(1)
    body = _build_sdp(udp_port, payload_type, sprop_parameter_sets)
    if GROUND_SDP_PATH.exists():
        try:
            if GROUND_SDP_PATH.read_text() == body:
                return GROUND_SDP_PATH
        except OSError:
            pass
    GROUND_SDP_PATH.write_text(body)
    return GROUND_SDP_PATH


def _parse_h264_annexb_nals(blob: bytes) -> list[bytes]:
    """Split an Annex-B H.264 bitstream into NAL unit bodies.

    The input is the raw payload ffmpeg writes when asked for
    ``-f h264 -`` — start codes ``00 00 00 01`` or ``00 00 01`` followed
    by the NAL unit. Returns the NAL bodies (the byte starting with the
    NAL header, e.g. ``0x67`` for SPS). Empty / malformed start-code
    runs are skipped.
    """
    out: list[bytes] = []
    n = len(blob)
    i = 0
    last_start = -1
    while i < n - 3:
        if blob[i] == 0 and blob[i + 1] == 0:
            if blob[i + 2] == 1:
                start = i + 3
            elif blob[i + 2] == 0 and blob[i + 3] == 1:
                start = i + 4
            else:
                i += 1
                continue
            if last_start >= 0:
                out.append(blob[last_start:i])
            last_start = start
            i = start
        else:
            i += 1
    if last_start >= 0 and last_start < n:
        out.append(blob[last_start:n])
    return out


def _extract_sps_pps_from_nals(nals: list[bytes]) -> tuple[bytes, bytes] | None:
    """Return the first (SPS, PPS) pair found in a NAL-unit list.

    NAL type lives in the low 5 bits of the first NAL byte. Type 7 is
    SPS, type 8 is PPS. We take the first occurrence of each — the
    encoder emits the same SPS/PPS on every IDR so any pair is the
    canonical one for the stream.
    """
    sps: bytes | None = None
    pps: bytes | None = None
    for nal in nals:
        if not nal:
            continue
        nal_type = nal[0] & 0x1F
        if nal_type == 7 and sps is None:
            sps = nal
        elif nal_type == 8 and pps is None:
            pps = nal
        if sps is not None and pps is not None:
            return sps, pps
    return None


async def _probe_sprop_parameter_sets(rtsp_url: str) -> str | None:
    """Probe the live mediamtx RTSP stream and return the sprop string.

    Spawns a short-lived ffmpeg that captures a few seconds of the
    Annex-B H.264 bitstream from ``rtsp_url`` (the local mediamtx
    publisher session, NOT the upstream UDP source — going through
    mediamtx avoids competing with the live ffmpeg ingest for the UDP
    port). Parses the bitstream for the first SPS + PPS NAL pair,
    base64-encodes both per RFC 6184 §8.1, and returns
    ``"<b64sps>,<b64pps>"``. Returns ``None`` if ffmpeg fails, the
    bitstream contains no SPS/PPS within the probe window, or any
    other error path — callers fall back to the no-sprop SDP.
    """
    binary = shutil.which("ffmpeg")
    if not binary:
        return None
    cmd = [
        binary,
        "-hide_banner",
        "-loglevel", "error",
        "-rtsp_transport", "tcp",
        "-i", rtsp_url,
        "-c:v", "copy",
        "-t", str(SPROP_PROBE_DURATION_SECONDS),
        "-f", "h264",
        "-",
    ]
    try:
        proc = await asyncio.create_subprocess_exec(
            *cmd,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.DEVNULL,
        )
    except Exception as exc:  # noqa: BLE001
        log.warning("sprop_probe_spawn_failed", error=str(exc))
        return None
    try:
        stdout, _ = await asyncio.wait_for(
            proc.communicate(),
            timeout=SPROP_PROBE_DURATION_SECONDS + 5.0,
        )
    except TimeoutError:
        try:
            proc.kill()
            await proc.wait()
        except ProcessLookupError:
            pass
        log.warning("sprop_probe_timeout")
        return None
    if not stdout:
        return None
    nals = _parse_h264_annexb_nals(stdout)
    pair = _extract_sps_pps_from_nals(nals)
    if pair is None:
        log.warning(
            "sprop_probe_no_parameter_sets",
            nal_count=len(nals),
            bytes_captured=len(stdout),
        )
        return None
    sps, pps = pair
    sprop = (
        base64.b64encode(sps).decode("ascii")
        + ","
        + base64.b64encode(pps).decode("ascii")
    )
    log.info(
        "sprop_probe_succeeded",
        sps_bytes=len(sps),
        pps_bytes=len(pps),
        sprop=sprop,
    )
    return sprop


async def bake_sprop_into_sdp(
    rtsp_url: str,
    udp_port: int,
    payload_type: int = GROUND_RTP_PAYLOAD_TYPE,
) -> None:
    """One-shot SDP bake task.

    Cheap path first: if the SDP already carries a sprop-parameter-
    sets value, return immediately without spawning the probe. The
    bake fires once after a clean install (when the SDP has no
    sprop) and never again. Every other ffmpeg restart short-
    circuits here.

    Slow path: probe the live stream after ffmpeg + mediamtx
    settle, extract SPS + PPS, and update /etc/ados/wfb/video.sdp
    in place.

    Why the cheap path matters: the probe spawns a second ffmpeg
    that connects to the local mediamtx as a reader. On a memory-
    constrained SBC under swap pressure, serving the publisher
    and the probe-reader concurrently stalls the publisher's RTSP
    read goroutine, which trips the frame-counter watchdog, which
    recycles ffmpeg, which re-enters this method — a feedback
    loop that observed publisher uptime drop from ~15 min to
    ~3 min on the Pi 4B 1 GB bench rig.
    """
    try:
        await asyncio.sleep(SPROP_PROBE_DELAY_SECONDS)
        # Cheap path: SDP already baked.
        try:
            existing_sdp = GROUND_SDP_PATH.read_text()
        except OSError:
            existing_sdp = ""
        existing_match = re.search(
            r"sprop-parameter-sets=([^\s;]+)", existing_sdp
        )
        if existing_match is not None and existing_match.group(1):
            log.debug(
                "ground_sprop_bake_skipped_already_present",
                sprop=existing_match.group(1),
            )
            return
        # Slow path: probe the live stream.
        sprop = await _probe_sprop_parameter_sets(rtsp_url)
        if not sprop:
            log.warning("ground_sprop_bake_no_sprop_extracted")
            return
        try:
            _write_sdp(
                udp_port,
                payload_type,
                sprop_parameter_sets=sprop,
            )
        except OSError as exc:
            log.error(
                "ground_sprop_bake_write_failed",
                error=str(exc),
            )
            return
        log.info(
            "ground_sprop_bake_complete",
            sdp_path=str(GROUND_SDP_PATH),
        )
    except asyncio.CancelledError:
        raise
    except Exception as exc:  # noqa: BLE001
        log.warning("ground_sprop_bake_failed", error=str(exc))


__all__ = [
    "GROUND_INGEST_UDP_PORT",
    "GROUND_RTSP_PATH",
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
]
