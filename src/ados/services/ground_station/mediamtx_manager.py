"""mediamtx lifecycle for the ground-station profile.

The air-side mediamtx (ados.services.video.mediamtx.MediamtxManager)
ingests a local camera encoder and publishes WHEP. On the ground side
the ingest source is different: wfb_rx decodes the radio stream and
pushes RTP-framed H.264 to UDP 127.0.0.1:5600. Everything else
(WHEP republish, ICE config, stderr draining, process lifecycle) is
identical, so this module reuses `MediamtxManager` and only swaps in
a ground-profile config generator plus an ffmpeg ingest helper.

Data flow:

    wfb_rx  -->  udp://127.0.0.1:5600  (RTP-framed H.264, payload type 96)
        |
        v
    ffmpeg (-i sdp:..., -c copy)  -->  rtsp://127.0.0.1:8554/main
        |
        v
    mediamtx (publisher source on /main)  -->  WHEP at :8889/ados/whep
        |
        v
    Browser GCS / Android app

Why RTP and not raw H.264: the wfb-ng wire protocol broadcasts each UDP
datagram as one 802.11 frame with FEC. A datagram lost beyond FEC capacity
must not corrupt the rest of the stream. RTP carries one NAL fragment per
packet and re-syncs at the next packet; raw H.264 over UDP loses bytes
mid-NAL and the decoder cannot recover until the next start code. The
upstream wfb-ng README explicitly mandates "RTP packet with video or
audio" as the UDP payload (README §design line 6, line 138, line 150).

The SDP file at /etc/ados/wfb/video.sdp tells ffmpeg the RTP stream's
encoding (H.264 / 90kHz / packetization-mode 1) without any RTSP
DESCRIBE round-trip, since wfb_rx is a one-way broadcast.
"""

from __future__ import annotations

import asyncio
import base64
import re
import shutil
import signal
import sys
import tempfile
import time
from pathlib import Path

import structlog
import yaml

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

from ados.core.config import load_config
from ados.core.logging import configure_logging, get_logger
from ados.services.video.mediamtx import MediamtxManager, _detect_lan_ips

log = get_logger("ground_station.mediamtx")

GROUND_INGEST_UDP_PORT = 5600
GROUND_RTSP_PATH = "main"
GROUND_WHEP_PATH = "ados/whep"
GROUND_RTP_PAYLOAD_TYPE = 96
# SDP describing the RTP stream wfb_rx pushes to UDP 5600. ffmpeg reads
# this file via `-f sdp -i ...` so it knows the codec / clock rate /
# packetization mode without an RTSP DESCRIBE round-trip (wfb_rx is a
# one-way broadcaster, no RTSP server to query). We write a fresh copy
# from generate_config() each time to track port / payload-type changes.
# Sits next to /etc/ados/wfb/{rx,tx}.key in the same writable dir.
GROUND_SDP_PATH = Path("/etc/ados/wfb/video.sdp")


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


# Number of seconds to wait for ffmpeg + mediamtx to be healthy before
# probing the live stream for SPS/PPS NAL units.
SPROP_PROBE_DELAY_SECONDS = 6.0

# How long to read the H.264 bitstream from mediamtx before giving up.
# Both SPS and PPS are emitted at every IDR, and the encoder's GOP is
# fps/2 (~500 ms), so a 3 s window catches at least 4 IDR boundaries —
# more than enough to find SPS + PPS even if the first ones arrive at
# a half-second offset from the probe start.
SPROP_PROBE_DURATION_SECONDS = 3.0


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
    except asyncio.TimeoutError:
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


class MediamtxGsManager:
    """Ground-profile wrapper around the shared MediamtxManager.

    Holds one `MediamtxManager` for the RTSP/WHEP server and one ffmpeg
    subprocess that bridges UDP 5600 into the server on `/main`.
    """

    def __init__(
        self,
        api_port: int = 9997,
        rtsp_port: int = 8554,
        webrtc_port: int = 8889,
        udp_ingest_port: int = GROUND_INGEST_UDP_PORT,
    ) -> None:
        self._core = MediamtxManager(
            api_port=api_port,
            rtsp_port=rtsp_port,
            webrtc_port=webrtc_port,
        )
        self._udp_port = udp_ingest_port
        self._ffmpeg: asyncio.subprocess.Process | None = None
        self._ffmpeg_stderr_task: asyncio.Task | None = None
        self._config_path: str = ""
        self._running = False
        # TX-liveness tracking. ffmpeg emits `frame=NNNN ...` progress
        # lines on stderr. The stderr drain parses out N and updates
        # these two fields; the supervisor in main() compares the wall
        # time since the last advance against a stall threshold so a
        # publisher whose downstream RTSP write has wedged (mediamtx
        # back-pressure / broken pipe build-up) is reaped before the
        # broken-pipe restart cascade kicks in.
        self._ffmpeg_frame_count: int = 0
        self._ffmpeg_last_frame_at: float = 0.0
        # Background task that probes the live RTSP session for SPS +
        # PPS NAL units once after each ffmpeg start and bakes them
        # into the SDP. See _bake_sprop_into_sdp.
        self._sprop_bake_task: asyncio.Task | None = None

    @property
    def running(self) -> bool:
        return self._running

    @property
    def rtsp_port(self) -> int:
        return self._core.rtsp_port

    @property
    def webrtc_port(self) -> int:
        return self._core.webrtc_port

    def generate_config(self) -> str:
        """Write a ground-profile mediamtx YAML to a temp file.

        Same base shape as the air-side generator but the `/main` path is
        declared with `source: publisher` so ffmpeg can push into it. The
        WHEP path `ados/whep` is aliased to the same source.
        """
        lan_ips = _detect_lan_ips()
        log.info("ground_mediamtx_webrtc_hosts", hosts=lan_ips)

        config: dict = {
            "logLevel": "warn",
            "api": True,
            "apiAddress": f":{self._core._api_port}",
            "rtsp": True,
            "rtspAddress": f":{self._core._rtsp_port}",
            # Per-reader write queue depth, measured in RTP packets
            # (not bytes). When a reader's queue overflows, mediamtx
            # logs "reader is too slow, discarding N frames" and the
            # reader's stream becomes corrupted — the canonical
            # browser-WebRTC "freeze on last frame, refresh restores"
            # symptom. Headroom math: at ~30 fps and worst-case
            # 50 RTP packets per frame for 1280x720 H.264 = 1500
            # packets/sec; 4096 holds ~2.7 s of buffer. Survives a
            # routine Chrome GC pause, a paint frame, a tab focus
            # change, and a brief Pi 4B swap-in stall — all the
            # things the earlier 512-packet ceiling caught and
            # turned into reader eviction. Memory cost: ~5 MB per
            # active reader. Acceptable.
            "writeQueueSize": 4096,
            # NB: do NOT override readTimeout / writeTimeout below
            # the gortsplib defaults (10 s / 10 s). A 5 s ceiling
            # under low-RAM swap pressure caught system stutters
            # the 10 s default absorbs, and produced a deterministic
            # 120 s publisher eviction cycle on Pi 4B 1 GB boards.
            # The 10 s default gives the kernel enough room to page
            # mediamtx's working set back in without tearing the
            # publisher session.
            "udpMaxPayloadSize": 1472,
            "webrtc": True,
            "webrtcAddress": f":{self._core._webrtc_port}",
            "webrtcAllowOrigin": "*",
            "webrtcIPsFromInterfaces": False,
            "webrtcIPsFromInterfacesList": [],
            "webrtcHandshakeTimeout": "15s",
            "webrtcLocalUDPAddress": (
                f"{lan_ips[0]}:8189" if lan_ips else ":8189"
            ),
            "webrtcLocalTCPAddress": (
                f"{lan_ips[0]}:8189" if lan_ips else ":8189"
            ),
            "webrtcICEServers2": [
                {"url": "stun:stun.l.google.com:19302"},
                {"url": "stun:stun1.l.google.com:19302"},
                {"url": "stun:stun2.l.google.com:19302"},
                {"url": "stun:stun.cloudflare.com:3478"},
                {"url": "stun:global.stun.twilio.com:3478"},
            ],
            # LL-HLS as a parallel transport. mediamtx serves the same
            # /main path simultaneously over WebRTC and HLS. WebRTC's
            # failure mode under load is "freeze on last frame and
            # require renegotiation"; HLS's failure mode is "drift
            # latency by a second, keep playing." Browsers that hit
            # a WebRTC freeze can fall back to the HLS endpoint at
            # http://<host>:8888/<path>/index.m3u8 and keep watching.
            # Works with the existing -c copy publish path — mediamtx
            # remuxes H.264 NAL units into fragmented MP4 without
            # re-encoding. Target latency on LAN: ~600 ms. Memory
            # cost: ~10-15 MB extra for the segmenter.
            "hls": True,
            "hlsAddress": ":8888",
            "hlsAllowOrigin": "*",
            "hlsAlwaysRemux": True,
            "hlsVariant": "lowLatency",
            "hlsSegmentCount": 7,
            "hlsSegmentDuration": "200ms",
            "hlsPartDuration": "100ms",
            "paths": {
                # ffmpeg pushes RTSP here with -c copy from udp://:5600
                GROUND_RTSP_PATH: {"source": "publisher"},
                # WHEP alias. sourceOnDemand True keeps the secondary
                # internal reader idle until a WHEP client actually
                # connects to /ados/whep. The earlier `False` setting
                # ran a permanent self-pull of /main into /ados/whep
                # even when no one was using the WHEP path (browsers
                # hit /main directly via webrtc:8889 endpoint), which
                # doubled mediamtx's internal goroutine count and
                # held a second RTSP reader open against the publisher
                # all the time. The cold-start delay on WHEP first
                # connect is ~1 GOP (~500 ms at fps/2 keyint); a fair
                # trade for half the routine load.
                GROUND_WHEP_PATH: {
                    "source": f"rtsp://127.0.0.1:{self._core._rtsp_port}/{GROUND_RTSP_PATH}",
                    "sourceOnDemand": True,
                },
            },
        }
        if lan_ips:
            config["webrtcAdditionalHosts"] = lan_ips

        config_dir = Path(tempfile.gettempdir()) / "ados"
        config_dir.mkdir(parents=True, exist_ok=True)
        config_path = config_dir / "mediamtx-gs.yml"

        with open(config_path, "w") as f:
            yaml.dump(config, f, default_flow_style=False)

        self._config_path = str(config_path)
        # Piggyback onto the core manager's config state so its start()
        # knows where to read from.
        self._core._config_path = self._config_path

        # Drop the RTP-describing SDP next to /etc/ados/wfb so the
        # ffmpeg ingest can read it via `-f sdp -i ...`. wfb_rx is a
        # one-way broadcaster — there is no RTSP server to DESCRIBE —
        # so the codec parameters (H264 / 90 kHz / packetization-mode 1)
        # must come from a static descriptor.
        try:
            sdp_path = _write_sdp(self._udp_port, GROUND_RTP_PAYLOAD_TYPE)
            log.info(
                "ground_sdp_written",
                path=str(sdp_path),
                payload_type=GROUND_RTP_PAYLOAD_TYPE,
            )
        except OSError as exc:
            log.error(
                "ground_sdp_write_failed",
                path=str(GROUND_SDP_PATH),
                error=str(exc),
            )

        log.info(
            "ground_mediamtx_config_generated",
            path=self._config_path,
            udp_ingest=self._udp_port,
        )
        return self._config_path

    async def _start_ffmpeg_ingest(self) -> bool:
        """Spawn ffmpeg that reads RTP from UDP 5600 and publishes to mediamtx.

        Reads via `-f sdp -i <path>` so ffmpeg knows the codec without
        an RTSP DESCRIBE round-trip (wfb_rx is a one-way broadcaster,
        no RTSP server to query). `-c copy` keeps it zero-transcode;
        the h264_mp4toannexb bsf re-flags the bitstream as Annex-B for
        the downstream RTSP push.
        """
        binary = shutil.which("ffmpeg")
        if not binary:
            log.error("ffmpeg_not_found", msg="ffmpeg not in PATH")
            return False

        if not GROUND_SDP_PATH.exists():
            # generate_config() should have written this; if it didn't
            # (e.g., a config regen race), retry now so we never spawn
            # ffmpeg without an SDP to read from.
            try:
                _write_sdp(self._udp_port, GROUND_RTP_PAYLOAD_TYPE)
            except OSError as exc:
                log.error(
                    "ground_sdp_missing_and_unwritable",
                    path=str(GROUND_SDP_PATH),
                    error=str(exc),
                )
                return False

        rtsp_url = (
            f"rtsp://127.0.0.1:{self._core._rtsp_port}/{GROUND_RTSP_PATH}"
        )
        # `-protocol_whitelist file,udp,rtp` is required because the SDP
        # path schemes (file for the SDP itself, udp+rtp for the media)
        # must all be explicitly allowed when reading via `sdp://`.
        # Default probesize/analyzeduration so ffmpeg can tolerate a
        # silent socket at boot before pairing completes.
        cmd = [
            binary,
            "-fflags", "nobuffer",
            "-flags", "low_delay",
            # NB: do NOT add `-max_delay 0` here. We tried it as a
            # latency micro-optimization and it broke codec discovery
            # — ffmpeg returned "Could not find codec parameters for
            # stream 0 (Video: h264, none): unspecified size" because
            # the flag overrode the probesize/analyzeduration window.
            # The codec params (width/height/profile/level) only
            # arrive inline in the first IDR, which can take a couple
            # of seconds after wfb_rx hands over the first packets.
            "-protocol_whitelist", "file,udp,rtp",
            # `-probesize 5M -analyzeduration 5M` give ffmpeg up to
            # 5 seconds (or 5 MB) to discover the H.264 SPS/PPS from
            # the incoming RTP stream. The SDP carries only the
            # encoding name + clock rate; codec config (width/height/
            # profile/level) arrives inline in the first IDR.
            # NB: bench validation surfaced a race when this was
            # tightened to 1M/1s — even with the drone encoder at
            # keyint=15 (IDR every 500 ms), the first RTP packets
            # landing in ffmpeg's parser are mid-GOP P-frames with
            # no SPS/PPS, and ffmpeg threw `decode_slice_header
            # error` + `unspecified size` before an IDR arrived.
            # 20M/20s is the safety margin: cold restarts under load
            # (Pi 4B class, swap pressure, GOP > 1 s) sometimes wait
            # past 5 s for the first IDR and the older 5M/5s caused
            # an `unspecified size` death loop with mandatory 5 s
            # backoff between each retry. 20 s is invisible on a
            # healthy boot because probe exits as soon as an IDR is
            # found, not after the full window.
            "-probesize", "20M",
            "-analyzeduration", "20M",
            # RTP demuxer reorder + max-delay window. ffmpeg's default
            # `max_delay = 500_000` (500 ms) is what produces the
            # `[sdp @ ...] max delay reached. need to consume packet`
            # + `RTP: missed N packets` cascade on any sub-second
            # system stutter (swap thrash, USB sysfs walk, kernel
            # task-group rebalance). Widening to 2 s lets the demuxer
            # absorb a brief stall and resume without dropping a
            # whole burst of packets. `reorder_queue_size` is the
            # max number of packets held for reorder; bumping from
            # the libav default 500 to a matched 256 keeps the
            # buffer bounded for a 4 Mbps live stream.
            "-max_delay", "2000000",
            "-reorder_queue_size", "256",
            "-f", "sdp",
            "-i", str(GROUND_SDP_PATH),
            "-c:v", "copy",
            # NO h264_mp4toannexb here: rtph264depay already emits
            # Annex-B (start-code-prefixed) NAL units; the bsf was a
            # leftover from the old `-f h264 -i udp://` path that
            # received raw bytes. Applying it twice corrupts the
            # bitstream's NAL boundaries.
            # `-muxdelay 0 -muxpreload 0 -flush_packets 1` strip
            # ffmpeg's default 0.7 s mux delay + 0.5 s preload +
            # output-side packet aggregation; for live RTSP push we
            # want every packet emitted as soon as encoded.
            "-muxdelay", "0",
            "-muxpreload", "0",
            "-flush_packets", "1",
            "-f", "rtsp",
            "-rtsp_transport", "tcp",
            rtsp_url,
        ]

        try:
            self._ffmpeg = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.PIPE,
            )
            # Reset liveness counters so the new ffmpeg gets a fresh
            # stall window starting from the spawn moment, not from
            # whatever the previous process left behind.
            self._ffmpeg_frame_count = 0
            self._ffmpeg_last_frame_at = time.monotonic()
            self._ffmpeg_stderr_task = asyncio.create_task(
                self._drain_ffmpeg_stderr()
            )
            log.info(
                "ground_ffmpeg_ingest_started",
                pid=self._ffmpeg.pid,
                udp_port=self._udp_port,
                rtsp=rtsp_url,
            )
            # Kick off the sprop bake as fire-and-forget. The probe
            # waits for ffmpeg + mediamtx to be healthy, captures a
            # short stretch of the live bitstream, extracts the SPS +
            # PPS NAL pair, and rewrites the SDP so subsequent ingest
            # restarts come up with parameter sets in the SDP and
            # mediamtx serves them out-of-band in the WHEP SDP. This
            # closes the Chrome WebRTC freeze-after-sync-loss path.
            if (
                self._sprop_bake_task is None
                or self._sprop_bake_task.done()
            ):
                self._sprop_bake_task = asyncio.create_task(
                    self._bake_sprop_into_sdp(rtsp_url),
                    name="ground_sprop_bake",
                )
            return True
        except Exception as exc:
            log.error("ground_ffmpeg_start_failed", error=str(exc))
            return False

    async def _bake_sprop_into_sdp(self, rtsp_url: str) -> None:
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
                    self._udp_port,
                    GROUND_RTP_PAYLOAD_TYPE,
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

    async def _drain_ffmpeg_stderr(self) -> None:
        """Drain ffmpeg stderr and surface error lines to the journal.

        Also parses the `frame=NNNN` progress token from each line and
        updates the TX-liveness counters so the monitor in main() can
        catch a wedged publisher before mediamtx's RTSP write-side
        breaks the pipe (the speed-decay -> broken-pipe pattern).
        """
        if self._ffmpeg is None or self._ffmpeg.stderr is None:
            return
        try:
            while True:
                # readuntil() on `\r` keeps each carriage-return-
                # terminated progress record as its own line. ffmpeg
                # uses `\r` for in-place progress; readline() (which
                # stops at `\n`) would batch many records into one
                # giant string and we'd only see the freshest one
                # whenever the journal eventually flushed.
                try:
                    chunk = await self._ffmpeg.stderr.readuntil(b"\r")
                except asyncio.IncompleteReadError as exc:
                    chunk = exc.partial
                    if not chunk:
                        break
                except asyncio.LimitOverrunError:
                    # readuntil's default StreamReader buffer is 64 KB;
                    # an absurdly long progress line shouldn't happen
                    # in practice, but fall back to a bounded read so
                    # we never deadlock the drain.
                    chunk = await self._ffmpeg.stderr.read(4096)
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
                        latest = self._ffmpeg_frame_count
                    if latest > self._ffmpeg_frame_count:
                        self._ffmpeg_frame_count = latest
                        self._ffmpeg_last_frame_at = time.monotonic()
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

    def ffmpeg_frame_stalled(self, window_s: float = FFMPEG_FRAME_STALL_SECONDS) -> bool:
        """True when ffmpeg's frame counter has not advanced for window_s.

        Caller (the monitor loop) treats a True return as authorization
        to terminate the ffmpeg subprocess and restart it. The check is
        skipped while no process is alive — the dead-process path is
        handled by the existing `ffmpeg_alive()` branch.
        """
        if not self.ffmpeg_alive():
            return False
        # Cold start: counters were just reset in _start_ffmpeg_ingest.
        # Give the encoder probe window plus a safety margin before we
        # consider a zero-frame state a stall. The `-analyzeduration`
        # ffmpeg flag is set to 20 s above; FFMPEG_FRAME_STALL_SECONDS
        # is 8 s. Allow up to (probe + stall) before the very first
        # frame is required.
        first_frame_grace = 28.0
        if self._ffmpeg_frame_count == 0:
            since_start = time.monotonic() - self._ffmpeg_last_frame_at
            return since_start >= first_frame_grace
        return (time.monotonic() - self._ffmpeg_last_frame_at) >= window_s

    def ffmpeg_frame_count(self) -> int:
        """Latest `frame=` value observed in ffmpeg's stderr."""
        return self._ffmpeg_frame_count

    async def start(self) -> bool:
        """Start mediamtx and the ffmpeg ingest."""
        if not self._config_path:
            self.generate_config()

        ok = await self._core.start()
        if not ok:
            return False

        # Wait for mediamtx's RTSP listener to actually accept TCP before
        # spawning the ffmpeg ingest. On slow SBCs (Pi 4B post-reboot)
        # mediamtx takes 5-15 s to bind 8554 even after the parent
        # process is up, and the previous fixed 0.5 s sleep was not
        # enough — ffmpeg's first publish attempt got "Connection
        # refused" and exited, leaving the health monitor to chase a
        # moving target.
        from ados.services.video.mediamtx import _wait_for_tcp_port

        ready = await _wait_for_tcp_port(
            "127.0.0.1", self._core._rtsp_port, timeout_s=30.0
        )
        if not ready:
            log.error(
                "ground_mediamtx_rtsp_not_ready",
                port=self._core._rtsp_port,
                timeout_s=30.0,
            )
            await self._core.stop()
            return False

        ingest_ok = await self._start_ffmpeg_ingest()
        if not ingest_ok:
            await self._core.stop()
            return False

        self._running = True
        log.info("ground_mediamtx_ready")
        return True

    async def stop(self) -> None:
        """Stop ffmpeg first, then mediamtx."""
        self._running = False

        if self._sprop_bake_task is not None:
            self._sprop_bake_task.cancel()
            self._sprop_bake_task = None

        if self._ffmpeg_stderr_task is not None:
            self._ffmpeg_stderr_task.cancel()
            self._ffmpeg_stderr_task = None

        if self._ffmpeg is not None and self._ffmpeg.returncode is None:
            try:
                self._ffmpeg.terminate()
                await asyncio.wait_for(self._ffmpeg.wait(), timeout=5.0)
            except TimeoutError:
                self._ffmpeg.kill()
                await self._ffmpeg.wait()
            except ProcessLookupError:
                pass
        self._ffmpeg = None

        await self._core.stop()

        if self._config_path:
            try:
                Path(self._config_path).unlink(missing_ok=True)
            except OSError:
                pass
        log.info("ground_mediamtx_stopped")

    def is_running(self) -> bool:
        if not self._running:
            return False
        core_alive = self._core.is_running()
        ffmpeg_alive = (
            self._ffmpeg is not None and self._ffmpeg.returncode is None
        )
        return core_alive and ffmpeg_alive

    def to_dict(self) -> dict:
        base = self._core.to_dict()
        base["ffmpeg_running"] = (
            self._ffmpeg is not None and self._ffmpeg.returncode is None
        )
        base["udp_ingest_port"] = self._udp_port
        base["whep_path"] = GROUND_WHEP_PATH
        return base

    def ffmpeg_alive(self) -> bool:
        """True when the UDP-to-RTSP ffmpeg sidecar process is running."""
        return self._ffmpeg is not None and self._ffmpeg.returncode is None

    async def restart_ffmpeg(self) -> bool:
        """Reap the dead ffmpeg sidecar and spawn a fresh one.

        Used by the health monitor in `main()` so a sidecar that exited
        (e.g., because mediamtx's RTSP port was not yet listening on
        the first attempt) doesn't leave mediamtx without a publisher
        forever. Waits for the RTSP port to actually accept TCP again
        before respawning so the new ffmpeg doesn't immediately hit
        the same "Connection refused" the previous one died on.
        """
        if self._ffmpeg_stderr_task is not None:
            self._ffmpeg_stderr_task.cancel()
            self._ffmpeg_stderr_task = None
        if self._ffmpeg is not None:
            if self._ffmpeg.returncode is None:
                try:
                    self._ffmpeg.terminate()
                    await asyncio.wait_for(self._ffmpeg.wait(), timeout=3.0)
                except (TimeoutError, ProcessLookupError):
                    try:
                        self._ffmpeg.kill()
                    except ProcessLookupError:
                        pass
            self._ffmpeg = None
        from ados.services.video.mediamtx import _wait_for_tcp_port

        ready = await _wait_for_tcp_port(
            "127.0.0.1", self._core._rtsp_port, timeout_s=10.0
        )
        if not ready:
            log.warning(
                "ground_mediamtx_rtsp_not_ready_on_restart",
                port=self._core._rtsp_port,
            )
            return False
        return await self._start_ffmpeg_ingest()


async def main() -> None:
    """Service entry point. Invoked by systemd via `python -m`."""
    config = load_config()
    configure_logging(config.logging.level)
    slog = structlog.get_logger()
    slog.info("ground_mediamtx_service_starting")

    shutdown = asyncio.Event()
    loop = asyncio.get_event_loop()
    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, shutdown.set)

    manager = MediamtxGsManager()
    ok = await manager.start()
    if not ok:
        slog.error("ground_mediamtx_start_failed")
        sys.exit(2)

    slog.info("ground_mediamtx_service_ready")

    # Monitor the ffmpeg sidecar that ingests UDP 5600 -> RTSP push.
    # The first attempt at boot can exit because wfb_rx hasn't received
    # any radio frames yet (UDP 5600 silent, ffmpeg's probe gives up).
    # Without this loop, mediamtx ends up with no publisher and the
    # ground-station path stays empty forever even after pairing
    # completes and the radio starts delivering.
    async def _monitor_ffmpeg() -> None:
        backoff = 5.0
        max_backoff = 60.0
        while not shutdown.is_set():
            try:
                await asyncio.wait_for(
                    shutdown.wait(), timeout=FFMPEG_MONITOR_TICK_SECONDS
                )
                return
            except asyncio.TimeoutError:
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

    monitor_task = asyncio.create_task(_monitor_ffmpeg(), name="ffmpeg_monitor")

    await shutdown.wait()

    slog.info("ground_mediamtx_service_stopping")
    monitor_task.cancel()
    try:
        await monitor_task
    except (asyncio.CancelledError, Exception):
        pass
    await manager.stop()
    slog.info("ground_mediamtx_service_stopped")


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
    sys.exit(0)
