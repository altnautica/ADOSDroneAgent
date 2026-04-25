"""mediamtx lifecycle for the ground-station profile.

The air-side mediamtx (ados.services.video.mediamtx.MediamtxManager)
ingests a local camera encoder and publishes WHEP. On the ground side
the ingest source is different: wfb_rx decodes the radio stream and
pushes a raw H.264 payload to UDP 127.0.0.1:5600. Everything else
(WHEP republish, ICE config, stderr draining, process lifecycle) is
identical, so this module reuses `MediamtxManager` and only swaps in
a ground-profile config generator plus an ffmpeg ingest helper.

Data flow:

    wfb_rx  -->  udp://127.0.0.1:5600  (H.264 Annex-B, no MPEG-TS wrapper)
        |
        v
    ffmpeg (copy, -f mpegts -> -f rtsp)  -->  rtsp://127.0.0.1:8554/main
        |
        v
    mediamtx (publisher source on /main)  -->  WHEP at :8889/ados/whep
        |
        v
    Browser GCS / Android app

Why the ffmpeg hop: wfb_rx emits a bare payload, not an RTSP or RTP
stream. mediamtx cannot ingest bare UDP without a container. ffmpeg
does the muxing with `-c copy` so there is zero transcoding cost.
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
                # WHEP alias. mediamtx serves WHEP on any published path,
                # so the GCS URL is /ados/whep when this path publishes.
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
        log.info(
            "ground_mediamtx_config_generated",
            path=self._config_path,
            udp_ingest=self._udp_port,
        )
        return self._config_path

    async def _start_ffmpeg_ingest(self) -> bool:
        """Spawn ffmpeg that reads UDP 5600 and publishes to mediamtx.

        Uses `-c copy` (no transcode) and the H.264 mp4-to-annexb bsf
        for consistent NAL delivery, matching the fix pattern from the
        air-side video pipeline.
        """
        binary = shutil.which("ffmpeg")
        if not binary:
            log.error("ffmpeg_not_found", msg="ffmpeg not in PATH")
            return False

        rtsp_url = (
            f"rtsp://127.0.0.1:{self._core._rtsp_port}/{GROUND_RTSP_PATH}"
        )
        cmd = [
            binary,
            "-fflags", "nobuffer",
            "-flags", "low_delay",
            "-probesize", "32",
            "-analyzeduration", "0",
            "-f", "h264",
            "-i", f"udp://@:{self._udp_port}?fifo_size=1000000&overrun_nonfatal=1",
            "-c:v", "copy",
            "-bsf:v", "h264_mp4toannexb",
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
        """Drain ffmpeg stderr to prevent pipe buffer deadlock."""
        if self._ffmpeg is None or self._ffmpeg.stderr is None:
            return
        try:
            while True:
                line = await self._ffmpeg.stderr.readline()
                if not line:
                    break
                text = line.decode(errors="replace").rstrip()
                if text:
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

        # Give mediamtx a beat to open its RTSP port before the ingest
        # tries to publish. The core manager already sleeps 1s but an
        # extra short wait here avoids a transient "connection refused"
        # on slow SBCs.
        await asyncio.sleep(0.5)

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

    await shutdown.wait()

    slog.info("ground_mediamtx_service_stopping")
    await manager.stop()
    slog.info("ground_mediamtx_service_stopped")


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
    sys.exit(0)
