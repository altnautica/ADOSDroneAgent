"""mediamtx + ffmpeg-ingest lifecycle for the ground-station profile.

The air-side mediamtx (``ados.services.video.mediamtx.MediamtxManager``)
ingests a local camera encoder and publishes WHEP. On the ground side
the ingest source is different: wfb_rx decodes the radio stream and
pushes RTP-framed H.264 to UDP 127.0.0.1:5600. Everything else
(WHEP republish, ICE config, stderr draining, process lifecycle) is
identical, so this module reuses ``MediamtxManager`` and only swaps in
a ground-profile config generator plus an ffmpeg ingest helper.

Data flow::

    wfb_rx  -->  udp://127.0.0.1:5600  (RTP-framed H.264, payload type 96)
        |
        v
    ffmpeg (-i sdp:..., -c copy)  -->  rtsp://127.0.0.1:8554/main
        |
        v
    mediamtx (publisher source on /main)  -->  WHEP at :8889/main/whep
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

The SDP file at ``/etc/ados/wfb/video.sdp`` tells ffmpeg the RTP
stream's encoding (H.264 / 90 kHz / packetization-mode 1) without any
RTSP DESCRIBE round-trip, since wfb_rx is a one-way broadcast.
"""

from __future__ import annotations

import asyncio
import shutil
import signal
import sys
import tempfile
import time
from pathlib import Path

import structlog
import yaml

from ados.core.config import load_config
from ados.core.logging import configure_logging, get_logger
from ados.services.video.mediamtx import MediamtxManager, _detect_lan_ips

from .ffmpeg_monitor import (
    FFMPEG_FRAME_STALL_SECONDS,
    drain_ffmpeg_stderr,
)
from .process_argv import build_ffmpeg_ingest_argv, build_mediamtx_yaml
from .rtsp_config import (
    GROUND_INGEST_UDP_PORT,
    GROUND_RTP_PAYLOAD_TYPE,
    GROUND_RTSP_PATH,
    GROUND_SDP_PATH,
    _write_sdp,
    bake_sprop_into_sdp,
)
from .tx_watchdog import monitor_ffmpeg, wfb_source_signal

log = get_logger("ground_station.mediamtx")


class MediamtxGsManager:
    """Ground-profile wrapper around the shared MediamtxManager.

    Holds one ``MediamtxManager`` for the RTSP/WHEP server and one
    ffmpeg subprocess that bridges UDP 5600 into the server on
    ``/main``.
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
        # TX-liveness tracking. Two signals:
        #   1. /proc/<ffmpeg_pid>/io wchar — cumulative write() bytes.
        #      Advances on every RTSP socket send regardless of how
        #      chatty ffmpeg's stderr is at the moment. Primary signal.
        #   2. ffmpeg's `frame=NNNN` stderr progress lines, parsed by
        #      the stderr drain. Fallback when /proc/<pid>/io is gated
        #      (kernel.yama.ptrace_scope hardening, rare on the rigs
        #      we run but kept for resilience).
        # The supervisor in main() compares the wall time since the
        # last advance against a stall threshold so a publisher whose
        # downstream RTSP write has wedged (mediamtx back-pressure /
        # broken pipe build-up) is reaped before the broken-pipe
        # restart cascade kicks in. Per Rule 37, a kernel counter
        # delta is preferred over parsing subprocess stderr because
        # delayed log flushes can starve the text parser even while
        # the process is healthy.
        self._ffmpeg_frame_count: int = 0
        self._ffmpeg_last_frame_at: float = 0.0
        self._ffmpeg_last_wchar: int = -1
        self._ffmpeg_last_wchar_at: float = 0.0
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

        Delegates to :func:`build_mediamtx_yaml` for the dict body and
        owns only the disk write + side-file SDP refresh.
        """
        lan_ips = _detect_lan_ips()
        log.info("ground_mediamtx_webrtc_hosts", hosts=lan_ips)

        config = build_mediamtx_yaml(
            api_port=self._core._api_port,
            rtsp_port=self._core._rtsp_port,
            webrtc_port=self._core._webrtc_port,
            lan_ips=lan_ips,
        )

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

        Reads via ``-f sdp -i <path>`` so ffmpeg knows the codec without
        an RTSP DESCRIBE round-trip (wfb_rx is a one-way broadcaster,
        no RTSP server to query). ``-c copy`` keeps it zero-transcode;
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
        cmd = build_ffmpeg_ingest_argv(binary, GROUND_SDP_PATH, rtsp_url)

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
            self._ffmpeg_last_wchar = -1
            self._ffmpeg_last_wchar_at = time.monotonic()
            self._ffmpeg_stderr_task = asyncio.create_task(
                drain_ffmpeg_stderr(self._ffmpeg, self._record_frame)
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

    def _record_frame(self, latest: int) -> None:
        """Callback the stderr drain invokes when a fresher frame counter is seen."""
        if latest > self._ffmpeg_frame_count:
            self._ffmpeg_frame_count = latest
            self._ffmpeg_last_frame_at = time.monotonic()

    async def _bake_sprop_into_sdp(self, rtsp_url: str) -> None:
        """One-shot SDP bake task. Delegates to :func:`bake_sprop_into_sdp`."""
        await bake_sprop_into_sdp(
            rtsp_url=rtsp_url,
            udp_port=self._udp_port,
            payload_type=GROUND_RTP_PAYLOAD_TYPE,
        )

    def ffmpeg_frame_stalled(self, window_s: float = FFMPEG_FRAME_STALL_SECONDS) -> bool:
        """True when ffmpeg's frame counter has not advanced for window_s.

        Caller (the monitor loop) treats a True return as authorization
        to terminate the ffmpeg subprocess and restart it. The check is
        skipped while no process is alive — the dead-process path is
        handled by the existing ``ffmpeg_alive()`` branch.
        """
        if not self.ffmpeg_alive():
            return False
        # Primary signal: /proc/<ffmpeg_pid>/io wchar. Advances on
        # every write() ffmpeg does — including RTSP socket sends —
        # so a process actively pushing frames is detected as healthy
        # regardless of whether its stderr progress lines have made
        # it through the journal buffer yet. Same Rule-37 pattern as
        # the wfb_tx zombie watchdog: kernel counter delta > userspace
        # text parse.
        now = time.monotonic()
        wchar = self._read_ffmpeg_wchar()
        if wchar is not None:
            if self._ffmpeg_last_wchar == -1:
                self._ffmpeg_last_wchar = wchar
                self._ffmpeg_last_wchar_at = now
                # First sample — give it the cold-start grace window
                # so we don't false-positive before any frame has
                # been produced.
                first_frame_grace = 28.0
                since_start = now - self._ffmpeg_last_frame_at
                return since_start >= first_frame_grace
            if wchar > self._ffmpeg_last_wchar:
                self._ffmpeg_last_wchar = wchar
                self._ffmpeg_last_wchar_at = now
                return False
            return (now - self._ffmpeg_last_wchar_at) >= window_s

        # Fallback: /proc/<pid>/io unreadable. Use the stderr
        # frame-counter parse. Known false-positive risk on healthy
        # ffmpeg when the stderr drain is slow to flush; kept here
        # so a hardened kernel still has some signal.
        first_frame_grace = 28.0
        if self._ffmpeg_frame_count == 0:
            since_start = now - self._ffmpeg_last_frame_at
            return since_start >= first_frame_grace
        return (now - self._ffmpeg_last_frame_at) >= window_s

    def _read_ffmpeg_wchar(self) -> int | None:
        """Cumulative write() bytes for the live ffmpeg subprocess.

        Returns ``None`` when the process is gone, the file is
        unreadable, or the kernel gates ``/proc/<pid>/io`` (e.g.,
        ``kernel.yama.ptrace_scope`` tightened beyond what
        ``CAP_DAC_OVERRIDE`` covers). Caller falls back to the
        stderr-frame parser.
        """
        if self._ffmpeg is None or self._ffmpeg.pid is None:
            return None
        try:
            with open(f"/proc/{self._ffmpeg.pid}/io") as f:
                for line in f:
                    if line.startswith("wchar:"):
                        return int(line.split(":", 1)[1].strip())
        except (FileNotFoundError, PermissionError, OSError, ValueError):
            return None
        return None

    def ffmpeg_frame_count(self) -> int:
        """Latest ``frame=`` value observed in ffmpeg's stderr."""
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

        # Do not spawn the ffmpeg ingest into a silent radio. With no
        # drone paired / no frames on UDP 5600, ffmpeg blocks in its
        # codec probe forever (never exits, never publishes) and spins
        # CPU on an idle appliance. Defer the spawn when the receiver is
        # confirmed up-but-silent; the monitor loop brings the ingest up
        # within one tick of the first packet, so the live path's
        # glass-to-glass latency is unchanged when a source IS present.
        if wfb_source_signal() == "silent":
            self._running = True
            log.info(
                "ground_ffmpeg_ingest_deferred_no_source",
                msg=(
                    "radio receiver up but no frames; deferring ffmpeg "
                    "until a source appears"
                ),
            )
            return True

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
        return base

    def ffmpeg_alive(self) -> bool:
        """True when the UDP-to-RTSP ffmpeg sidecar process is running."""
        return self._ffmpeg is not None and self._ffmpeg.returncode is None

    async def path_has_publisher(self) -> bool:
        """True when mediamtx reports an active publisher on /main.

        Readiness signal for consumers (the LCD tap, the heartbeat) that
        want to know whether the ground path is actually serving video,
        not just whether the processes are alive. Queries the mediamtx
        API; returns False on any error so an unreachable API reads as
        "not ready" rather than a false positive.
        """
        import httpx

        api_port = self._core._api_port
        try:
            async with httpx.AsyncClient(
                base_url=f"http://127.0.0.1:{api_port}",
                timeout=httpx.Timeout(2.0, connect=0.5),
            ) as client:
                resp = await client.get(
                    f"/v3/paths/get/{GROUND_RTSP_PATH}"
                )
                if resp.status_code != 200:
                    return False
                data = resp.json()
                if not isinstance(data, dict):
                    return False
                return bool(data.get("ready", False)) and bool(
                    data.get("source")
                )
        except Exception:
            return False

    async def stop_ffmpeg_ingest(self) -> None:
        """Reap just the ffmpeg ingest sidecar, leaving mediamtx core up.

        Used by the monitor to stop an ffmpeg that is spinning in its
        codec probe against a silent radio (no source, no publisher).
        The RTSP/WHEP core stays up and ready; the monitor restarts the
        ingest the moment packets flow again. Distinct from ``stop()``,
        which tears the whole manager (core included) down for shutdown.
        """
        if self._sprop_bake_task is not None:
            self._sprop_bake_task.cancel()
            self._sprop_bake_task = None
        if self._ffmpeg_stderr_task is not None:
            self._ffmpeg_stderr_task.cancel()
            self._ffmpeg_stderr_task = None
        if self._ffmpeg is not None and self._ffmpeg.returncode is None:
            try:
                self._ffmpeg.terminate()
                await asyncio.wait_for(self._ffmpeg.wait(), timeout=3.0)
            except TimeoutError:
                try:
                    self._ffmpeg.kill()
                    await self._ffmpeg.wait()
                except ProcessLookupError:
                    pass
            except ProcessLookupError:
                pass
        self._ffmpeg = None

    async def restart_ffmpeg(self) -> bool:
        """Reap the dead ffmpeg sidecar and spawn a fresh one.

        Used by the health monitor in ``main()`` so a sidecar that exited
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
    """Service entry point. Invoked by systemd via ``python -m``."""
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

    monitor_task = asyncio.create_task(
        monitor_ffmpeg(manager, shutdown, slog),
        name="ffmpeg_monitor",
    )

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


__all__ = ["MediamtxGsManager", "main"]
