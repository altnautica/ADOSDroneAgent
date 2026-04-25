"""Video pipeline service — orchestrates camera, encoder, streaming, and recording."""

from __future__ import annotations

import asyncio
import os
import time
from enum import StrEnum
from typing import TYPE_CHECKING

from ados.core.logging import get_logger
from ados.hal.camera import discover_cameras
from ados.services.video.camera_mgr import CameraManager
from ados.services.video.encoder import (
    EncoderConfig,
    EncoderType,
    build_encoder_command,
    detect_encoder_for_camera,
)
from ados.services.video.mediamtx import MediamtxManager
from ados.services.video.recorder import VideoRecorder

if TYPE_CHECKING:
    from ados.core.config import VideoConfig

log = get_logger("video.pipeline")

_HEALTH_CHECK_INTERVAL = 5.0


class PipelineState(StrEnum):
    STOPPED = "stopped"
    STARTING = "starting"
    RUNNING = "running"
    ERROR = "error"


class VideoPipeline:
    """Orchestrates the full video pipeline: camera -> encoder -> stream.

    Manages subprocess lifecycle, health checks, and integrates with
    the camera manager, encoder, mediamtx, and recorder.
    """

    # Grace period after pipeline start before health checks run.
    # ffmpeg needs ~1-2s to open the camera, encode the first frame, and
    # connect to mediamtx RTSP. Without this grace period, the very first
    # health check fires <0.5s after start, finds mediamtx path not ready
    # (ffmpeg hasn't published yet), and kills a perfectly healthy pipeline.
    _STARTUP_GRACE_SECS: float = 8.0

    def __init__(self, config: VideoConfig) -> None:
        self._config = config
        self._state = PipelineState.STOPPED
        self._camera_mgr = CameraManager()
        self._recorder = VideoRecorder(config.recording.path)
        self._mediamtx = MediamtxManager()
        self._encoder_process: asyncio.subprocess.Process | None = None
        self._encoder_type: EncoderType | None = None
        self._cloud_push_process: asyncio.subprocess.Process | None = None
        self._cloud_stderr_task: asyncio.Task | None = None
        # Encoder stderr is captured (not DEVNULL) so ffmpeg errors surface.
        self._encoder_stderr_task: asyncio.Task | None = None
        self._restart_count: int = 0
        self._cloud_restart_count: int = 0
        self._max_restart_delay: float = 300.0  # 5 minutes
        self._base_restart_delay: float = 5.0
        self._started_at: float = 0.0  # monotonic time of last start_stream()

    @property
    def state(self) -> PipelineState:
        return self._state

    @property
    def camera_manager(self) -> CameraManager:
        return self._camera_mgr

    @property
    def recorder(self) -> VideoRecorder:
        return self._recorder

    @property
    def mediamtx(self) -> MediamtxManager:
        return self._mediamtx

    def _discover_and_assign(self) -> None:
        """Run camera discovery and auto-assign roles."""
        cameras = discover_cameras()
        self._camera_mgr.set_cameras(cameras)
        self._camera_mgr.auto_assign()

    async def start_stream(self) -> bool:
        """Start the encoding and streaming pipeline.

        Returns True if the stream started successfully.
        """
        if self._state == PipelineState.RUNNING:
            log.warning("pipeline_already_running")
            return True

        # Clean up any leftover processes from a previous run
        if self._encoder_process is not None and self._encoder_process.returncode is None:
            log.info("killing_stale_encoder", pid=self._encoder_process.pid)
            self._encoder_process.kill()
            await self._encoder_process.wait()
            self._encoder_process = None

        self._state = PipelineState.STARTING

        # Discover cameras
        self._discover_and_assign()

        primary = self._camera_mgr.get_primary()
        if not primary:
            log.error("no_primary_camera")
            self._state = PipelineState.ERROR
            return False

        # Detect encoder
        self._encoder_type = detect_encoder_for_camera(primary)
        if self._encoder_type is None:
            log.error("no_encoder_available")
            self._state = PipelineState.ERROR
            return False

        # Build encoder command
        enc_config = EncoderConfig(
            type=self._encoder_type,
            codec=self._config.camera.codec,
            width=self._config.camera.width,
            height=self._config.camera.height,
            fps=self._config.camera.fps,
            bitrate_kbps=self._config.camera.bitrate_kbps,
        )

        # Start mediamtx for stream output
        pipe_uri = f"rtsp://localhost:{self._mediamtx.rtsp_port}/main"
        cmd = build_encoder_command(enc_config, primary.device_path, pipe_uri, camera=primary)

        if not cmd:
            log.error("encoder_command_empty")
            self._state = PipelineState.ERROR
            return False

        # Configure and start mediamtx
        self._mediamtx.generate_config({"main": "publisher"})
        mtx_ok = await self._mediamtx.start()
        if not mtx_ok:
            log.error("mediamtx_start_failed", msg="cannot stream without mediamtx — install mediamtx")
            self._state = PipelineState.ERROR
            return False

        # Start encoder subprocess. Pipe stderr and drain in the
        # background so ffmpeg errors surface in the structured log
        # rather than getting silently dropped on crash.
        try:
            self._encoder_process = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.PIPE,
            )
            self._encoder_stderr_task = asyncio.create_task(
                self._drain_stderr(self._encoder_process, "encoder")
            )
            self._state = PipelineState.RUNNING
            self._started_at = time.monotonic()
            log.info(
                "pipeline_started",
                encoder=self._encoder_type.value,
                camera=primary.name,
            )
            return True
        except FileNotFoundError:
            log.error("encoder_binary_not_found", encoder=self._encoder_type.value)
            self._state = PipelineState.ERROR
            return False

    @staticmethod
    async def _drain_stderr(proc: asyncio.subprocess.Process, label: str) -> None:
        """Continuously drain subprocess stderr to prevent pipe buffer deadlock.

        Logs at WARNING level so ffmpeg errors are visible in journalctl
        at the default info log level. Previously logged at debug, which
        hid every ffmpeg crash reason from the operator.
        """
        if proc.stderr is None:
            return
        try:
            while True:
                line = await proc.stderr.readline()
                if not line:
                    break
                text = line.decode(errors="replace").rstrip()
                if text:
                    log.warning("subprocess_stderr", label=label, line=text)
        except (asyncio.CancelledError, Exception):
            pass

    async def start_cloud_push(self) -> bool:
        """Push local RTSP stream to cloud video relay for remote viewing.

        Spawns an ffmpeg process that reads from local mediamtx RTSP
        and pushes to the cloud relay RTSP endpoint. Uses TCP transport
        and timeouts to detect network failures.
        """
        cloud_url = self._config.cloud_relay_url
        if not cloud_url:
            log.info("cloud_push_disabled", reason="no cloud_relay_url configured")
            return False

        if self._state != PipelineState.RUNNING:
            log.warning("cloud_push_skipped", reason="pipeline not running")
            return False

        local_rtsp = f"rtsp://localhost:{self._mediamtx.rtsp_port}/main"
        push_url = f"{cloud_url}/main"

        try:
            self._cloud_push_process = await asyncio.create_subprocess_exec(
                "ffmpeg",
                "-rtsp_transport", "tcp",
                "-timeout", "5000000",
                "-i", local_rtsp,
                "-c", "copy",
                "-f", "rtsp",
                "-rtsp_transport", "tcp",
                push_url,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.PIPE,
            )
            # Drain stderr in background to prevent pipe buffer deadlock
            self._cloud_stderr_task = asyncio.create_task(
                self._drain_stderr(self._cloud_push_process, "cloud_push")
            )
            log.info("cloud_push_started", destination=push_url)
            return True
        except FileNotFoundError:
            log.error("cloud_push_ffmpeg_not_found")
            return False

    async def stop_cloud_push(self) -> None:
        """Stop the cloud RTSP push."""
        if self._cloud_stderr_task is not None:
            self._cloud_stderr_task.cancel()
            self._cloud_stderr_task = None
        proc = self._cloud_push_process
        if proc is not None and proc.returncode is None:
            proc.terminate()
            try:
                await asyncio.wait_for(proc.wait(), timeout=5.0)
            except (TimeoutError, asyncio.CancelledError):
                proc.kill()
            self._cloud_push_process = None
            log.info("cloud_push_stopped")

    async def stop_stream(self) -> None:
        """Stop the encoding pipeline and mediamtx."""
        log.info("stop_stream_begin")
        await self.stop_cloud_push()

        # The encoder subprocess could already be dead by the time
        # stop_stream() runs (e.g. ffmpeg crashed 5s after start due to
        # h264_v4l2m2m device-not-found). Calling .terminate() /
        # .kill() / .wait() on a dead process raises ProcessLookupError
        # from asyncio's base_subprocess._check_proc, which used to
        # crash the video service. Guard every call with
        # `returncode is None` and swallow ProcessLookupError.
        if self._encoder_stderr_task is not None:
            self._encoder_stderr_task.cancel()
            self._encoder_stderr_task = None

        proc = self._encoder_process
        if proc is not None:
            # Check if the process still exists at all. os.kill(pid, 0)
            # raises ProcessLookupError if the PID is gone, which means
            # the child was already reaped. In that case, skip proc.wait()
            # entirely — asyncio's proc.wait() can hang forever if the
            # SIGCHLD was already consumed before the event loop's child
            # watcher could track it. This was the root cause of
            # stop_stream() hanging indefinitely.
            pid_alive = True
            if proc.pid is not None:
                try:
                    os.kill(proc.pid, 0)
                except (ProcessLookupError, OSError):
                    pid_alive = False
                    log.info("encoder_already_dead", pid=proc.pid)

            if pid_alive and proc.returncode is None:
                try:
                    proc.terminate()
                except ProcessLookupError:
                    pass
                try:
                    await asyncio.wait_for(proc.wait(), timeout=5.0)
                except (TimeoutError, ProcessLookupError, asyncio.CancelledError):
                    if proc.returncode is None:
                        try:
                            proc.kill()
                        except ProcessLookupError:
                            pass
            # Don't call proc.wait() for dead processes — it hangs.
            self._encoder_process = None

        await self._mediamtx.stop()

        if self._recorder.recording:
            await self._recorder.stop_recording()

        self._state = PipelineState.STOPPED
        log.info("pipeline_stopped")

    async def _check_health(self) -> bool:
        """Check if the encoder and mediamtx are both running and healthy."""
        if self._encoder_process is None:
            return False
        if self._encoder_process.returncode is not None:
            log.warning(
                "encoder_process_exited",
                returncode=self._encoder_process.returncode,
            )
            return False
        # os.kill(pid, 0) detects dead processes even when asyncio hasn't
        # collected the exit code. This catches the case where ffmpeg dies
        # silently and proc.returncode stays None.
        if self._encoder_process.pid is not None:
            try:
                os.kill(self._encoder_process.pid, 0)
            except (ProcessLookupError, OSError):
                log.warning("encoder_process_vanished", pid=self._encoder_process.pid)
                return False
        # Also verify mediamtx is alive — if it crashes, ffmpeg blocks on its
        # TCP write to the dead RTSP socket and appears healthy (returncode is
        # still None), but no frames reach the browser.
        if not self._mediamtx.is_running():
            log.warning("mediamtx_died_during_stream")
            return False
        # Grace period: ffmpeg needs ~1-2s to open the camera, encode the
        # first frame, and connect to mediamtx RTSP. Without this, the
        # very first health check fires <0.5s after start, finds
        # mediamtx_path_not_ready (ffmpeg hasn't published yet), and kills
        # a perfectly healthy pipeline. Confirmed from journalctl: pipeline
        # starts at T+0s, health check kills it at T+0.45s.
        elapsed = time.monotonic() - self._started_at
        if elapsed < self._STARTUP_GRACE_SECS:
            return True
        # Verify mediamtx is actually receiving data from the encoder.
        # ffmpeg's RTSP TCP connection can silently die (e.g., during system
        # load spikes from service restarts). ffmpeg stays alive (process
        # returncode is None) but writes to a dead socket. mediamtx shows
        # the path as ready=false with no source. Detect this by probing
        # the mediamtx REST API.
        if not await self._check_mediamtx_path_ready():
            log.warning("mediamtx_path_not_ready", msg="encoder RTSP connection likely dead")
            return False
        return True

    async def _check_mediamtx_path_ready(self) -> bool:
        """Probe mediamtx API to verify the stream path has an active publisher.

        Returns False when the API is unreachable, returns an error, or
        reports no active publisher. Previous versions returned True on
        exceptions (assuming healthy when unable to check), which hid
        failures where mediamtx had crashed or the stream was dead.
        """
        import httpx
        import logging
        # Suppress httpx's per-request INFO log ("HTTP Request: GET ...")
        # which spams journalctl every 5 seconds with no diagnostic value.
        logging.getLogger("httpx").setLevel(logging.WARNING)
        try:
            async with httpx.AsyncClient(timeout=2.0) as client:
                resp = await client.get(
                    f"http://127.0.0.1:{self._mediamtx._api_port}/v3/paths/list"
                )
                if resp.status_code != 200:
                    return False
                data = resp.json()
                items = data.get("items", [])
                if not items:
                    return False
                return items[0].get("ready", False)
        except Exception:
            return False

    async def _check_cloud_push_health(self) -> bool:
        """Check if the cloud push subprocess is still running.

        Returns True if healthy or if cloud push is not configured.
        Returns False only when the process has died unexpectedly.
        """
        if self._cloud_push_process is None:
            return True  # Not configured, nothing to check
        if self._cloud_push_process.returncode is not None:
            log.warning(
                "cloud_push_process_exited",
                returncode=self._cloud_push_process.returncode,
            )
            self._cloud_push_process = None
            if self._cloud_stderr_task is not None:
                self._cloud_stderr_task.cancel()
                self._cloud_stderr_task = None
            return False
        return True

    async def run(self) -> None:
        """Main service loop — monitors pipeline health and restarts on failure.

        On cancellation, ensures the encoder subprocess is terminated and not
        orphaned (A-07).
        """
        log.info("video_pipeline_service_start")

        try:
            while True:
                if self._state == PipelineState.RUNNING:
                    if not await self._check_health():
                        self._restart_count += 1
                        delay = min(
                            self._base_restart_delay * (2 ** (self._restart_count - 1)),
                            self._max_restart_delay,
                        )
                        if self._restart_count >= 10:
                            log.error(
                                "pipeline_circuit_breaker",
                                msg="too many failures, waiting 5 minutes",
                                attempts=self._restart_count,
                            )
                            self._state = PipelineState.ERROR
                            await asyncio.sleep(self._max_restart_delay)
                            self._restart_count = 0
                            continue
                        log.warning(
                            "pipeline_health_check_failed",
                            msg="restarting",
                            attempt=self._restart_count,
                            backoff_secs=delay,
                        )
                        # Stop everything cleanly before restarting
                        await self.stop_stream()
                        await asyncio.sleep(max(0, delay - _HEALTH_CHECK_INTERVAL))
                        success = await self.start_stream()
                        if success:
                            self._restart_count = 0
                    elif not await self._check_cloud_push_health():
                        # Encoder is fine but cloud push died — restart only cloud push
                        self._cloud_restart_count += 1
                        delay = min(
                            self._base_restart_delay * (2 ** (self._cloud_restart_count - 1)),
                            self._max_restart_delay,
                        )
                        if self._cloud_restart_count >= 10:
                            log.error(
                                "cloud_push_circuit_breaker",
                                msg="too many cloud push failures, waiting 5 minutes",
                                attempts=self._cloud_restart_count,
                            )
                            await asyncio.sleep(self._max_restart_delay)
                            self._cloud_restart_count = 0
                        else:
                            log.warning(
                                "cloud_push_restarting",
                                attempt=self._cloud_restart_count,
                                backoff_secs=delay,
                            )
                            await self.stop_cloud_push()
                            await asyncio.sleep(max(0, delay - _HEALTH_CHECK_INTERVAL))
                            success = await self.start_cloud_push()
                            if success:
                                self._cloud_restart_count = 0

                elif self._state in (PipelineState.ERROR, PipelineState.STOPPED):
                    # Retry start_stream with backoff. Covers cases where the
                    # initial start failed (no camera at boot, missing binary)
                    # and the resource appears later (USB hotplug, apt install).
                    self._restart_count += 1
                    delay = min(
                        self._base_restart_delay * (2 ** (self._restart_count - 1)),
                        self._max_restart_delay,
                    )
                    if self._restart_count >= 10:
                        log.warning(
                            "pipeline_retry_backoff",
                            msg="10 consecutive failures, backing off 5 minutes",
                            attempts=self._restart_count,
                        )
                        await asyncio.sleep(self._max_restart_delay)
                        self._restart_count = 0
                        continue
                    log.info(
                        "pipeline_retry_from_error",
                        attempt=self._restart_count,
                        backoff_secs=delay,
                    )
                    await asyncio.sleep(max(0, delay - _HEALTH_CHECK_INTERVAL))
                    success = await self.start_stream()
                    if success:
                        self._restart_count = 0
                        log.info("pipeline_recovered", msg="stream started after retry")

                await asyncio.sleep(_HEALTH_CHECK_INTERVAL)
        finally:
            # Kill cloud push subprocess on shutdown/cancellation
            if self._cloud_push_process is not None and self._cloud_push_process.returncode is None:
                self._cloud_push_process.terminate()
                try:
                    await asyncio.wait_for(self._cloud_push_process.wait(), timeout=5.0)
                except (TimeoutError, asyncio.CancelledError):
                    self._cloud_push_process.kill()
                self._cloud_push_process = None
                log.info("cloud_push_process_cleaned_up")

            # Kill encoder subprocess on shutdown/cancellation to prevent orphans
            if self._encoder_process is not None and self._encoder_process.returncode is None:
                self._encoder_process.terminate()
                try:
                    await asyncio.wait_for(self._encoder_process.wait(), timeout=5.0)
                except (TimeoutError, asyncio.CancelledError):
                    self._encoder_process.kill()
                log.info("encoder_process_cleaned_up")
            await self._mediamtx.stop()

    def get_status(self) -> dict:
        """Return current pipeline status for API responses."""
        cloud_push = (
            self._cloud_push_process is not None
            and self._cloud_push_process.returncode is None
        )
        return {
            "state": self._state.value,
            "encoder": self._encoder_type.value if self._encoder_type else None,
            "cameras": self._camera_mgr.to_dict(),
            "recorder": self._recorder.to_dict(),
            "mediamtx": self._mediamtx.to_dict(),
            "cloud_push": cloud_push,
        }
