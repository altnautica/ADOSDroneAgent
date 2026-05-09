"""Video pipeline service — orchestrates camera, encoder, streaming, and recording."""

from __future__ import annotations

import asyncio
import logging
import os
import time
from enum import StrEnum
from typing import TYPE_CHECKING

import httpx

from ados.core.logging import get_logger
from ados.hal.camera import discover_cameras
from ados.services.video.camera_mgr import CameraManager, CameraRole
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

# Suppress httpx's per-request INFO log ("HTTP Request: GET ...") which
# spams journalctl every 5 seconds with no diagnostic value.
logging.getLogger("httpx").setLevel(logging.WARNING)

_HEALTH_CHECK_INTERVAL = 5.0

# Local UDP socket the wfb-ng radio reads from on the air side. The radio
# subprocess (`wfb_tx -u 5600 ...`) listens on this port and broadcasts
# each UDP datagram as a single 802.11 frame with FEC, per the wfb-ng
# protocol contract: every UDP datagram going in must be a self-contained
# unit that survives single-packet loss. We therefore wrap the encoded
# H.264 in RTP (RFC 6184) before handing it to wfb_tx — a lost RTP packet
# costs at most one NAL fragment, instead of corrupting the byte stream
# until the next start code (which is what raw-H.264-over-UDP does).
# Receiver wraps with rtph264depay; SDP at /etc/ados/wfb/video.sdp.
# pkt_size keeps each datagram under the 802.11 MTU after wfb-ng overhead.
_WFB_TEE_HOST = "127.0.0.1"
_WFB_TEE_PORT = 5600
_WFB_TEE_PKT_SIZE = 1316
_WFB_TEE_PAYLOAD_TYPE = 96
_WFB_TEE_SSRC = "0xCAFE"


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

    # Grace period after pipeline start before health checks declare the
    # pipeline dead. We exit grace as soon as mediamtx reports the path
    # has an active publisher (the "first packet" event), so fast boards
    # transition to live health checks in 1-2s while slow boards (Pi Zero,
    # cold camera open) get the full 30s window before being killed.
    # The previous fixed 8s wall-clock killed healthy slow boards.
    _STARTUP_GRACE_MAX_SECS: float = 30.0

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
        # ffmpeg sidecar that fans out the encoded RTSP back to UDP 5600
        # so the wfb-ng radio has H.264 to broadcast. Independent
        # lifecycle from cloud_push: cloud_push is for remote viewing,
        # the wfb tee is for the local radio link.
        self._wfb_tee_process: asyncio.subprocess.Process | None = None
        self._wfb_tee_stderr_task: asyncio.Task | None = None
        self._wfb_tee_restart_count: int = 0
        # Encoder stderr is captured (not DEVNULL) so ffmpeg errors surface.
        self._encoder_stderr_task: asyncio.Task | None = None
        self._restart_count: int = 0
        self._cloud_restart_count: int = 0
        self._max_restart_delay: float = 300.0  # 5 minutes
        self._base_restart_delay: float = 5.0
        self._started_at: float = 0.0  # monotonic time of last start_stream()
        self._first_packet_seen: bool = False  # set True once mediamtx reports a publisher
        # Stamp of the most recent healthy probe. Used to clear
        # `_restart_count` after a sustained run of healthy frames so a
        # transient failure during the day does not leave the counter
        # pinned and trigger the circuit breaker on the next dip.
        self._last_healthy_at: float = 0.0
        # Window of consecutive healthy time required to clear the
        # restart counter. Tuned for the operator-visible UX: a clip of
        # 60 s of clean frames is "we're back".
        self._healthy_reset_window_secs: float = 60.0
        # Reuse client across probes; mediamtx URL is stable.
        self._mediamtx_client: httpx.AsyncClient | None = None
        # Serializes camera-switch operations so two concurrent
        # restart_with_camera() calls cannot race each other and leave
        # the encoder pointed at one device while camera_mgr says
        # another.
        self._switch_lock = asyncio.Lock()

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
        #
        # If anything past mediamtx.start() raises, mediamtx must be torn
        # down too. Otherwise repeated start_stream() retries pile up
        # zombie mediamtx processes that collide on the same port.
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
            self._first_packet_seen = False
            log.info(
                "pipeline_started",
                encoder=self._encoder_type.value,
                camera=primary.name,
            )
            # Best-effort radio fan-out. Failure here does not fail the
            # pipeline; local mediamtx and cloud push still work, only
            # the radio link goes dark.
            await self.start_wfb_tee()
            return True
        except FileNotFoundError:
            log.error("encoder_binary_not_found", encoder=self._encoder_type.value)
            await self._teardown_after_partial_start()
            return False
        except Exception as exc:
            log.error("encoder_start_failed", error=str(exc), exc_info=True)
            await self._teardown_after_partial_start()
            return False

    async def _teardown_after_partial_start(self) -> None:
        """Roll back partial start. Stops any process spawned after mediamtx.start()."""
        # Sweep the wfb tee first; it depends on local RTSP being up.
        await self.stop_wfb_tee()
        # Encoder may have spawned but not been assigned cleanly; sweep it.
        if self._encoder_process is not None and self._encoder_process.returncode is None:
            try:
                self._encoder_process.kill()
                await asyncio.wait_for(self._encoder_process.wait(), timeout=2.0)
            except (TimeoutError, ProcessLookupError, OSError):
                pass
        self._encoder_process = None
        if self._encoder_stderr_task is not None:
            self._encoder_stderr_task.cancel()
            self._encoder_stderr_task = None
        # Tear down mediamtx so the next start_stream() is not blocked by
        # a zombie holding the port.
        try:
            await self._mediamtx.stop()
        except Exception as exc:
            log.warning("mediamtx_teardown_failed", error=str(exc))
        self._state = PipelineState.ERROR

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

    async def start_wfb_tee(self) -> bool:
        """Fan out the encoded H.264 RTSP back to local UDP for the radio.

        The wfb-ng TX subprocess listens on UDP 127.0.0.1:5600 for raw
        Annex-B H.264 frames to encapsulate and broadcast over the
        radio. Without something feeding that socket, the radio link is
        a dry pipe even when the encoder is publishing fine to mediamtx.
        This sidecar reads from the local mediamtx RTSP path with
        `-c:v copy` (no re-encode) and writes to UDP. On rigs without a
        wfb radio (e.g. a ground station running this service for some
        reason) the UDP packets are silently dropped by the kernel —
        harmless cost.
        """
        if self._state != PipelineState.RUNNING:
            log.warning("wfb_tee_skipped", reason="pipeline not running")
            return False

        # Idempotent: an existing healthy tee just stays.
        if (
            self._wfb_tee_process is not None
            and self._wfb_tee_process.returncode is None
        ):
            return True

        local_rtsp = f"rtsp://localhost:{self._mediamtx.rtsp_port}/main"
        rtp_url = (
            f"rtp://{_WFB_TEE_HOST}:{_WFB_TEE_PORT}?pkt_size={_WFB_TEE_PKT_SIZE}"
        )

        try:
            self._wfb_tee_process = await asyncio.create_subprocess_exec(
                "ffmpeg",
                "-fflags", "nobuffer",
                "-flags", "low_delay",
                "-rtsp_transport", "tcp",
                "-i", local_rtsp,
                "-c:v", "copy",
                "-f", "rtp",
                "-payload_type", str(_WFB_TEE_PAYLOAD_TYPE),
                "-ssrc", _WFB_TEE_SSRC,
                rtp_url,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.PIPE,
            )
            self._wfb_tee_stderr_task = asyncio.create_task(
                self._drain_stderr(self._wfb_tee_process, "wfb_tee")
            )
            log.info(
                "wfb_tee_started",
                source=local_rtsp,
                destination=rtp_url,
                payload_type=_WFB_TEE_PAYLOAD_TYPE,
                pid=self._wfb_tee_process.pid,
            )
            return True
        except FileNotFoundError:
            log.error("wfb_tee_ffmpeg_not_found")
            return False
        except Exception as exc:  # noqa: BLE001
            log.error("wfb_tee_start_failed", error=str(exc))
            return False

    async def stop_wfb_tee(self) -> None:
        """Stop the wfb radio UDP tee."""
        if self._wfb_tee_stderr_task is not None:
            self._wfb_tee_stderr_task.cancel()
            self._wfb_tee_stderr_task = None
        proc = self._wfb_tee_process
        if proc is not None and proc.returncode is None:
            try:
                proc.terminate()
            except ProcessLookupError:
                pass
            try:
                await asyncio.wait_for(proc.wait(), timeout=5.0)
            except (TimeoutError, asyncio.CancelledError):
                try:
                    proc.kill()
                except ProcessLookupError:
                    pass
            log.info("wfb_tee_stopped")
        self._wfb_tee_process = None

    async def stop_stream(self) -> None:
        """Stop the encoding pipeline and mediamtx."""
        log.info("stop_stream_begin")
        await self.stop_wfb_tee()
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

    def restart_attempts(self) -> int:
        """Public accessor for the encoder restart counter.

        Surfaced on the cloud heartbeat so the GCS health view can
        flag a flapping pipeline before the circuit breaker fires.
        """
        return self._restart_count

    def _note_healthy_tick(self, now: float | None = None) -> bool:
        """Stamp a healthy probe and clear the counter when stable.

        Returns True if the restart counter was just cleared as a
        result of this call. Carved out of `run()` so the reset
        decision can be tested without driving the infinite loop.
        """
        if now is None:
            now = time.monotonic()
        if self._last_healthy_at == 0.0:
            self._last_healthy_at = now
            return False
        if (
            self._restart_count > 0
            and now - self._last_healthy_at
            > self._healthy_reset_window_secs
        ):
            log.info(
                "pipeline_restart_counter_reset",
                msg="healthy window reached, clearing counter",
                window_secs=self._healthy_reset_window_secs,
                attempts=self._restart_count,
            )
            self._restart_count = 0
            return True
        return False

    def _note_unhealthy_tick(self) -> None:
        """Reset the consecutive-healthy timer on a failed probe."""
        self._last_healthy_at = 0.0

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
        elapsed = time.monotonic() - self._started_at
        # Grace logic: poll mediamtx during the grace window. The moment it
        # reports a publisher, latch _first_packet_seen and switch to live
        # health checks. On slow boards the camera open + first encode can
        # take 5-15 seconds; we allow up to _STARTUP_GRACE_MAX_SECS before
        # giving up. On fast boards we exit grace in 1-2 seconds.
        if not self._first_packet_seen:
            if await self._check_mediamtx_path_ready():
                self._first_packet_seen = True
                log.info("pipeline_first_packet", elapsed=round(elapsed, 1))
                return True
            if elapsed < self._STARTUP_GRACE_MAX_SECS:
                return True
            log.warning(
                "pipeline_grace_expired",
                msg="no mediamtx publisher after grace window",
                elapsed=round(elapsed, 1),
            )
            return False
        # Live health check: verify mediamtx is still receiving data from
        # the encoder. ffmpeg's RTSP TCP connection can silently die during
        # system load spikes; the process stays alive but writes to a dead
        # socket. mediamtx then reports ready=false.
        if not await self._check_mediamtx_path_ready():
            log.warning("mediamtx_path_not_ready", msg="encoder RTSP connection likely dead")
            return False
        return True

    async def _get_mediamtx_client(self) -> httpx.AsyncClient:
        """Lazily build the shared httpx client for mediamtx health probes."""
        if self._mediamtx_client is None:
            self._mediamtx_client = httpx.AsyncClient(
                base_url=f"http://127.0.0.1:{self._mediamtx._api_port}",
                timeout=httpx.Timeout(2.0, connect=0.5),
                limits=httpx.Limits(max_connections=2, max_keepalive_connections=1),
            )
        return self._mediamtx_client

    async def _close_mediamtx_client(self) -> None:
        """Tear down the shared httpx client on pipeline shutdown."""
        if self._mediamtx_client is not None:
            try:
                await self._mediamtx_client.aclose()
            except Exception as exc:
                log.warning("mediamtx_client_close_failed", error=str(exc))
            self._mediamtx_client = None

    async def _check_mediamtx_path_ready(self) -> bool:
        """Probe mediamtx API to verify the stream path has an active publisher.

        Returns False when the API is unreachable, returns an error, or
        reports no active publisher. Previous versions returned True on
        exceptions (assuming healthy when unable to check), which hid
        failures where mediamtx had crashed or the stream was dead.
        """
        try:
            client = await self._get_mediamtx_client()
            resp = await client.get("/v3/paths/list")
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

    async def _check_wfb_tee_health(self) -> bool:
        """Check if the wfb UDP tee subprocess is still running.

        Returns True if healthy or if the tee was never started.
        Returns False only when the process died unexpectedly so the
        run loop can restart it without flapping the encoder itself.
        """
        if self._wfb_tee_process is None:
            return True
        if self._wfb_tee_process.returncode is not None:
            log.warning(
                "wfb_tee_process_exited",
                returncode=self._wfb_tee_process.returncode,
            )
            self._wfb_tee_process = None
            if self._wfb_tee_stderr_task is not None:
                self._wfb_tee_stderr_task.cancel()
                self._wfb_tee_stderr_task = None
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
                    health_ok = await self._check_health()
                    if health_ok:
                        # Stamp the most recent healthy tick. After a
                        # sustained healthy window, clear any pinned
                        # restart counter so a fresh transient failure
                        # later in the day does not roll straight into
                        # the circuit breaker.
                        self._note_healthy_tick()
                    else:
                        # Restart the consecutive-healthy timer on any
                        # failed probe so a flap window has to start
                        # over before the counter clears.
                        self._note_unhealthy_tick()
                    if not health_ok:
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
                    elif not await self._check_wfb_tee_health():
                        # Encoder + cloud are fine but the radio fan-out died.
                        # Restart only the tee so the radio recovers without
                        # tearing down the rest of the pipeline.
                        self._wfb_tee_restart_count += 1
                        delay = min(
                            self._base_restart_delay * (2 ** (self._wfb_tee_restart_count - 1)),
                            self._max_restart_delay,
                        )
                        if self._wfb_tee_restart_count >= 10:
                            log.error(
                                "wfb_tee_circuit_breaker",
                                msg="too many wfb tee failures, waiting 5 minutes",
                                attempts=self._wfb_tee_restart_count,
                            )
                            await asyncio.sleep(self._max_restart_delay)
                            self._wfb_tee_restart_count = 0
                        else:
                            log.warning(
                                "wfb_tee_restarting",
                                attempt=self._wfb_tee_restart_count,
                                backoff_secs=delay,
                            )
                            await self.stop_wfb_tee()
                            await asyncio.sleep(max(0, delay - _HEALTH_CHECK_INTERVAL))
                            success = await self.start_wfb_tee()
                            if success:
                                self._wfb_tee_restart_count = 0

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
            # Kill the wfb tee first because it has a TCP connection to
            # local mediamtx that needs to drain before mediamtx itself
            # goes away.
            if self._wfb_tee_process is not None and self._wfb_tee_process.returncode is None:
                try:
                    self._wfb_tee_process.terminate()
                except ProcessLookupError:
                    pass
                try:
                    await asyncio.wait_for(self._wfb_tee_process.wait(), timeout=5.0)
                except (TimeoutError, asyncio.CancelledError):
                    try:
                        self._wfb_tee_process.kill()
                    except ProcessLookupError:
                        pass
                self._wfb_tee_process = None
                log.info("wfb_tee_process_cleaned_up")

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
            await self._close_mediamtx_client()

    def get_status(self) -> dict:
        """Return current pipeline status for API responses."""
        cloud_push = (
            self._cloud_push_process is not None
            and self._cloud_push_process.returncode is None
        )
        wfb_tee = (
            self._wfb_tee_process is not None
            and self._wfb_tee_process.returncode is None
        )
        return {
            "state": self._state.value,
            "encoder": self._encoder_type.value if self._encoder_type else None,
            "cameras": self._camera_mgr.to_dict(),
            "recorder": self._recorder.to_dict(),
            "mediamtx": self._mediamtx.to_dict(),
            "cloud_push": cloud_push,
            "wfb_tee": wfb_tee,
        }

    async def restart_with_camera(self, role: str, device_path: str) -> None:
        """Reassign a camera role and restart the encoder pointing at it.

        Drives the operator-initiated camera switch flow:

        * Validates the role against :class:`CameraRole`.
        * Locates the matching :class:`CameraInfo` in the camera manager
          and binds it to the requested role.
        * If a recording is active when ``role`` is ``primary``, gracefully
          stops the in-flight capture, restarts the encoder against the
          new device, and resumes recording into a new file. The result
          is two real MP4 files on disk: one ending at the switch boundary
          and one starting fresh after the encoder restart.
        * Tears down the current encoder + mediamtx subprocesses and
          starts a fresh stream so the new camera becomes the publisher.

        Concurrent calls are serialized through ``self._switch_lock``.
        """
        try:
            role_enum = CameraRole(role)
        except ValueError as exc:  # pragma: no cover - guarded by API
            raise ValueError(f"unknown camera role: {role}") from exc

        cameras = self._camera_mgr.cameras
        target = next(
            (c for c in cameras if c.device_path == device_path),
            None,
        )
        if target is None:
            raise LookupError(
                f"device_path {device_path} not present in enumerated cameras"
            )

        async with self._switch_lock:
            previous = self._camera_mgr.get_by_role(role_enum)
            from_path = previous.device_path if previous is not None else None

            # Capture the active recording state before we touch the
            # encoder so we can rotate the file across the switch.
            was_recording = self._recorder.recording
            previous_recording_path = (
                self._recorder.current_path if was_recording else ""
            )

            # Bind the role first so any restart that hits start_stream()
            # picks the correct primary.
            self._camera_mgr.assign_role(target, role_enum)

            log.info(
                "pipeline_camera_switch_begin",
                role=role_enum.value,
                from_device_path=from_path,
                to_device_path=device_path,
                recording=was_recording,
            )

            # Rotate the recording boundary if a primary-role switch
            # interrupts an active capture. Stop the current segment so
            # ffmpeg flushes the MP4 trailer; we restart it after the
            # encoder is back up.
            if was_recording and role_enum == CameraRole.PRIMARY:
                try:
                    await self._recorder.stop_recording()
                except Exception as exc:  # noqa: BLE001
                    log.warning(
                        "pipeline_camera_switch_recorder_stop_failed",
                        error=str(exc),
                    )

            # Tear down the encoder + mediamtx so start_stream() spawns a
            # fresh pair pointing at the newly-assigned primary.
            try:
                await self.stop_stream()
            except Exception as exc:  # noqa: BLE001
                log.warning(
                    "pipeline_camera_switch_stop_failed",
                    error=str(exc),
                )

            # Reset the discover hook so the next start_stream() picks up
            # the new role assignment without re-running auto_assign(),
            # which would clobber the operator's choice.
            await self._restart_after_assign()

            # Resume recording on the post-switch encoder. The new file
            # is generated from the timestamp at this point. The
            # ``post-switch`` suffix prevents a collision with the
            # pre-switch file when the rotation happens inside the same
            # wall-clock second the recorder timestamps with.
            if was_recording and role_enum == CameraRole.PRIMARY:
                try:
                    new_path = await self._recorder.start_recording(
                        filename_suffix="post-switch"
                    )
                    log.info(
                        "pipeline_camera_switch_recorder_resumed",
                        previous_path=previous_recording_path,
                        new_path=new_path,
                    )
                except Exception as exc:  # noqa: BLE001
                    log.warning(
                        "pipeline_camera_switch_recorder_resume_failed",
                        error=str(exc),
                    )

            log.info(
                "pipeline_camera_switched",
                role=role_enum.value,
                from_device_path=from_path,
                to_device_path=device_path,
            )

    async def _restart_after_assign(self) -> bool:
        """Restart the stream without running camera auto-assignment.

        ``start_stream`` normally re-runs ``_discover_and_assign`` which
        clobbers the role bindings we just set. This wrapper bypasses
        that step so an operator-driven switch survives the restart.
        """
        # Mirror start_stream() but skip _discover_and_assign so the
        # role bindings we just set are not overwritten.
        if self._encoder_process is not None and self._encoder_process.returncode is None:
            log.info("killing_stale_encoder", pid=self._encoder_process.pid)
            self._encoder_process.kill()
            await self._encoder_process.wait()
            self._encoder_process = None

        self._state = PipelineState.STARTING

        primary = self._camera_mgr.get_primary()
        if not primary:
            log.error("no_primary_camera")
            self._state = PipelineState.ERROR
            return False

        self._encoder_type = detect_encoder_for_camera(primary)
        if self._encoder_type is None:
            log.error("no_encoder_available")
            self._state = PipelineState.ERROR
            return False

        enc_config = EncoderConfig(
            type=self._encoder_type,
            codec=self._config.camera.codec,
            width=self._config.camera.width,
            height=self._config.camera.height,
            fps=self._config.camera.fps,
            bitrate_kbps=self._config.camera.bitrate_kbps,
        )

        pipe_uri = f"rtsp://localhost:{self._mediamtx.rtsp_port}/main"
        cmd = build_encoder_command(
            enc_config, primary.device_path, pipe_uri, camera=primary
        )
        if not cmd:
            log.error("encoder_command_empty")
            self._state = PipelineState.ERROR
            return False

        self._mediamtx.generate_config({"main": "publisher"})
        mtx_ok = await self._mediamtx.start()
        if not mtx_ok:
            log.error("mediamtx_start_failed")
            self._state = PipelineState.ERROR
            return False

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
            self._first_packet_seen = False
            log.info(
                "pipeline_started",
                encoder=self._encoder_type.value,
                camera=primary.name,
            )
            await self.start_wfb_tee()
            return True
        except FileNotFoundError:
            log.error("encoder_binary_not_found", encoder=self._encoder_type.value)
            await self._teardown_after_partial_start()
            return False
        except Exception as exc:
            log.error("encoder_start_failed", error=str(exc), exc_info=True)
            await self._teardown_after_partial_start()
            return False
