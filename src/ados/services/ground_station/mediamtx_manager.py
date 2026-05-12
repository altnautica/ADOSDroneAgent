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
import shutil
import signal
import sys
import tempfile
from pathlib import Path

import structlog
import yaml

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


def _build_sdp(udp_port: int, payload_type: int) -> str:
    """Return the SDP body that describes the wfb_rx RTP stream.

    Stripped back to the minimal H.264 over RTP/AVP descriptor —
    ffmpeg's -f sdp ingest opens RTCP on RTP+1 by default, and
    the LCD render-tap previously claimed that port. We've moved
    the LCD tap to 5605 (see ground_station/wfb_rx.py), freeing
    udp_port+1 for ffmpeg's default RTCP socket, so no explicit
    a=rtcp hint is needed.
    """
    return (
        "v=0\n"
        "o=- 0 0 IN IP4 127.0.0.1\n"
        "s=ADOS Video\n"
        "c=IN IP4 127.0.0.1\n"
        "t=0 0\n"
        f"m=video {udp_port} RTP/AVP {payload_type}\n"
        f"a=rtpmap:{payload_type} H264/90000\n"
        f"a=fmtp:{payload_type} packetization-mode=1\n"
    )


def _write_sdp(udp_port: int, payload_type: int = GROUND_RTP_PAYLOAD_TYPE) -> Path:
    """Write the SDP to GROUND_SDP_PATH and return the path. Idempotent."""
    GROUND_SDP_PATH.parent.mkdir(parents=True, exist_ok=True)
    body = _build_sdp(udp_port, payload_type)
    if GROUND_SDP_PATH.exists():
        try:
            if GROUND_SDP_PATH.read_text() == body:
                return GROUND_SDP_PATH
        except OSError:
            pass
    GROUND_SDP_PATH.write_text(body)
    return GROUND_SDP_PATH


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
            # readTimeout 5s + writeQueueSize 2048: the default RTSP
            # write queue is sized for typical streaming workloads; on
            # an air-link with bursty FEC blocks we observed mediamtx
            # dropping clients when a brief stall caused the queue to
            # fill. Doubling the queue size eats a few MB of RAM and
            # buys headroom across the inevitable wifi retries.
            "writeQueueSize": 2048,
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
            "hls": False,
            "paths": {
                # ffmpeg pushes RTSP here with -c copy from udp://:5600
                GROUND_RTSP_PATH: {"source": "publisher"},
                # WHEP alias. mediamtx serves WHEP on the same source
                # path without re-pulling on demand: sourceOnDemand
                # false keeps the WebRTC track warm between client
                # connects so the first-frame delay on browser open
                # collapses from ~500 ms (re-establish RTSP pull +
                # buffer one GOP) to whatever the WHEP DTLS handshake
                # alone costs. The RAM cost on the GS is single-
                # digit megabytes — the source is already running for
                # the local UDP fanout / LCD tap anyway.
                GROUND_WHEP_PATH: {
                    "source": f"rtsp://127.0.0.1:{self._core._rtsp_port}/{GROUND_RTSP_PATH}",
                    "sourceOnDemand": False,
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
            # `-probesize 1M -analyzeduration 1000000` give ffmpeg up
            # to 1 second (or 1 MB) to discover the H.264 SPS/PPS from
            # the incoming RTP stream. The SDP carries only the
            # encoding name + clock rate; codec config (width/height/
            # profile/level) arrives inline in the first IDR. The
            # default probesize/analyzeduration is too aggressive and
            # ffmpeg exited with "unspecified size" before the IDR
            # arrived. The earlier 5M/5s value was conservative — it
            # bought cold-start headroom but cost up to 5 s of
            # first-frame wait. With the drone encoder at keyint=15
            # @ 30 fps (IDR every 500 ms per encoder.py:411), an IDR
            # is on the wire within 500-1000 ms of wfb_rx handing over
            # the first packets. 1 s is the safe floor; reducing
            # further re-triggers the documented codec-discovery
            # failure mode.
            "-probesize", "1M",
            "-analyzeduration", "1000000",
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
            self._ffmpeg_stderr_task = asyncio.create_task(
                self._drain_ffmpeg_stderr()
            )
            log.info(
                "ground_ffmpeg_ingest_started",
                pid=self._ffmpeg.pid,
                udp_port=self._udp_port,
                rtsp=rtsp_url,
            )
            return True
        except Exception as exc:
            log.error("ground_ffmpeg_start_failed", error=str(exc))
            return False

    async def _drain_ffmpeg_stderr(self) -> None:
        """Drain ffmpeg stderr and surface error lines to the journal.

        Previously logged everything at debug, which hid the actual
        reason ffmpeg exited (e.g., "Could not find codec parameters")
        from the operator's journalctl view. We now look for explicit
        error markers and bump those to warning so the next bench
        bringup doesn't have to guess. Routine progress lines stay at
        debug to keep the journal scanable.
        """
        if self._ffmpeg is None or self._ffmpeg.stderr is None:
            return
        try:
            while True:
                line = await self._ffmpeg.stderr.readline()
                if not line:
                    break
                text = line.decode(errors="replace").rstrip()
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
                await asyncio.wait_for(shutdown.wait(), timeout=5.0)
                return
            except asyncio.TimeoutError:
                pass
            if manager.ffmpeg_alive():
                # Healthy tick; reset the backoff so the next outage
                # restarts quickly.
                backoff = 5.0
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
