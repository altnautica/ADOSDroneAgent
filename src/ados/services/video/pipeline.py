"""Video pipeline service — orchestrates camera, encoder, streaming, and recording."""

from __future__ import annotations

import asyncio
from enum import StrEnum
from typing import TYPE_CHECKING

from ados.core.logging import get_logger
from ados.hal.camera import discover_cameras
from ados.services.video.camera_mgr import CameraManager
from ados.services.video.encoder import (
    EncoderConfig,
    EncoderType,
    build_encoder_command,
    detect_available_encoder,
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

    def __init__(self, config: VideoConfig) -> None:
        self._config = config
        self._state = PipelineState.STOPPED
        self._camera_mgr = CameraManager()
        self._recorder = VideoRecorder(config.recording.path)
        self._mediamtx = MediamtxManager()
        self._encoder_process: asyncio.subprocess.Process | None = None
        self._encoder_type: EncoderType | None = None
        self._cloud_push_process: asyncio.subprocess.Process | None = None

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

        self._state = PipelineState.STARTING

        # Discover cameras
        self._discover_and_assign()

        primary = self._camera_mgr.get_primary()
        if not primary:
            log.error("no_primary_camera")
            self._state = PipelineState.ERROR
            return False

        # Detect encoder
        self._encoder_type = detect_available_encoder()
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
        cmd = build_encoder_command(enc_config, primary.device_path, pipe_uri)

        if not cmd:
            log.error("encoder_command_empty")
            self._state = PipelineState.ERROR
            return False

        # Configure and start mediamtx
        self._mediamtx.generate_config({"main": "publisher"})
        mtx_ok = await self._mediamtx.start()
        if not mtx_ok:
            log.warning("mediamtx_start_failed", msg="streaming without mediamtx")

        # Start encoder subprocess
        try:
            self._encoder_process = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.PIPE,
            )
            self._state = PipelineState.RUNNING
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

    async def start_cloud_push(self) -> bool:
        """Push local RTSP stream to cloud video relay for remote viewing.

        Spawns an ffmpeg process that reads from local mediamtx RTSP
        and pushes to the cloud relay RTSP endpoint.
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
                "-i", local_rtsp,
                "-c", "copy",
                "-f", "rtsp",
                push_url,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.PIPE,
            )
            log.info("cloud_push_started", destination=push_url)
            return True
        except FileNotFoundError:
            log.error("cloud_push_ffmpeg_not_found")
            return False

    async def stop_cloud_push(self) -> None:
        """Stop the cloud RTSP push."""
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
        await self.stop_cloud_push()

        if self._encoder_process is not None:
            self._encoder_process.terminate()
            try:
                await asyncio.wait_for(self._encoder_process.wait(), timeout=5.0)
            except TimeoutError:
                self._encoder_process.kill()
                await self._encoder_process.wait()
            self._encoder_process = None

        await self._mediamtx.stop()

        if self._recorder.recording:
            await self._recorder.stop_recording()

        self._state = PipelineState.STOPPED
        log.info("pipeline_stopped")

    def _check_health(self) -> bool:
        """Check if the encoder subprocess is still running."""
        if self._encoder_process is None:
            return False
        if self._encoder_process.returncode is not None:
            log.warning(
                "encoder_process_exited",
                returncode=self._encoder_process.returncode,
            )
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
                    if not self._check_health():
                        log.warning("pipeline_health_check_failed", msg="restarting")
                        self._state = PipelineState.STOPPED
                        self._encoder_process = None
                        await self.start_stream()

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
