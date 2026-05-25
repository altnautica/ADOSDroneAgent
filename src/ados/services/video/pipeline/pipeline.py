"""Video pipeline orchestrator.

Owns the long-lived :class:`VideoPipeline` instance the supervisor runs
on the air side. Every coroutine here is a distinct lifecycle stage:
start / health check / restart / stop / camera switch / cloud relay
push / wfb tee. The class has no internal seam where the GStreamer
pipeline can be split cleanly without sequencing risk, so the helpers
that ARE pure (constants, regexes) live in sibling modules and the
main class stays in one place.

Patching contract
-----------------

The tests patch a handful of names on the package barrel
(``ados.services.video.pipeline``) — :data:`log`, ``discover_cameras``,
``detect_encoder_for_camera``, ``build_encoder_command``. To honour
those patches, this module routes every call to those four names
through the live package object via ``_pkg`` (resolved lazily inside
each method that uses them). Other names (mediamtx, recorder, etc.)
are imported normally because no test patches them at the package
path.
"""

from __future__ import annotations

import asyncio
import logging
import os
import signal
import sys
import time
from enum import StrEnum
from pathlib import Path
from typing import TYPE_CHECKING, Any

import httpx

from ados.hal.detect import detect_board
from ados.services.video.air_pipeline import AirPipeline, AirPipelineUnavailable
from ados.services.video.camera_mgr import CameraManager, CameraRole
from ados.services.video.encoder import (
    EncoderConfig,
    EncoderType,
)
from ados.services.video.mediamtx import MediamtxManager
from ados.services.video.recorder import VideoRecorder

from .constants import (
    _HEALTH_CHECK_INTERVAL,
    _INBOUND_FLOW_STALL_SECONDS,
    _MEDIAMTX_MAIN_PATH,
)
from .discovery import _DiscoveryMixin
from .health import _HealthMixin
from .wfb_tee import _WfbTeeMixin

if TYPE_CHECKING:
    from ados.core.config import VideoConfig

# Suppress httpx's per-request INFO log ("HTTP Request: GET ...") which
# spams journalctl every 5 seconds with no diagnostic value.
logging.getLogger("httpx").setLevel(logging.WARNING)


def _pkg():
    """Return the package module so patched attributes resolve at call time.

    Tests do ``patch("ados.services.video.pipeline.discover_cameras",
    ...)`` which sets the attribute on the package's namespace. Reading
    those names through ``_pkg().<name>`` lets the patch take effect
    without any extra plumbing in the test layer.
    """
    return sys.modules["ados.services.video.pipeline"]


class PipelineState(StrEnum):
    STOPPED = "stopped"
    STARTING = "starting"
    RUNNING = "running"
    ERROR = "error"


class VideoPipeline(_DiscoveryMixin, _HealthMixin, _WfbTeeMixin):
    """Orchestrates the full video pipeline: camera -> encoder -> stream.

    Manages subprocess lifecycle, health checks, and integrates with
    the camera manager, encoder, mediamtx, and recorder.

    The class body intentionally keeps the orchestrator code (start /
    stop / health / restart loop) together; only the three pure-helper
    clusters (camera discovery, healthy-tick bookkeeping, wfb-tee
    subprocess + watchdog) are factored out into sibling mixin modules.
    See ``discovery.py``, ``health.py``, ``wfb_tee.py``. The warning
    in the module docstring still holds: ``_check_health``, ``run``,
    ``start_stream``, ``stop_stream``, and ``_start_air_pipeline``
    have sequencing dependencies that don't admit further extraction.
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
        # Output-progress watchdog state. Each time the wfb_tee stderr
        # emits a `frame=N` line with N greater than the previous one,
        # we stamp _wfb_tee_last_progress_at = time.monotonic(). The
        # health-check fails (triggering restart) if this stamp is
        # older than _WFB_TEE_PROGRESS_TIMEOUT_S. Catches the
        # "process alive but ffmpeg wedged" zombie mode that
        # process-liveness-only checks miss (Rule 37 contract).
        self._wfb_tee_last_progress_at: float = 0.0
        self._wfb_tee_last_frame_count: int = -1
        # Headless SEI tap. Reads the local mediamtx RTSP feed and
        # writes /run/ados/lcd-latency.json so the /api/video/latency
        # route returns numbers on a drone with no LCD attached.
        # Lazily constructed in start_stream when wfb.sei_latency is on.
        self._sei_tap: Any | None = None
        # Encoder stderr is captured (not DEVNULL) so ffmpeg errors surface.
        self._encoder_stderr_task: asyncio.Task | None = None
        self._restart_count: int = 0
        self._cloud_restart_count: int = 0
        self._max_restart_delay: float = 300.0  # 5 minutes
        # Tighter cap for "no_primary_camera" failures. Camera-not-present
        # is almost always a transient USB-hotplug condition (cable jiggle,
        # bus reset, mid-flight reseat) that resolves within seconds; a
        # 5-minute backoff makes the LCD look frozen when an operator is
        # actively plugging cameras in. Poll at human-attention cadence
        # for this case and keep the 5-minute cap for real driver wedges.
        self._max_restart_delay_no_camera: float = 30.0
        # Tag the most recent start_stream failure so the retry loop in
        # run() can pick the right backoff cap. None when the last start
        # succeeded or the cause is unknown.
        self._last_start_error: str | None = None
        # Signal-fired event that short-circuits the no_primary_camera
        # backoff sleep. The udev rule at
        # /etc/udev/rules.d/50-ados-uvc-no-autosuspend.rules sends
        # SIGUSR1 to ados-video on every fresh /dev/video* node; the
        # run() loop installs a signal handler that calls
        # _camera_plugged_event.set(), waking the retry from up to
        # 30 s of sleep down to ~1 s.
        self._camera_plugged_event: asyncio.Event = asyncio.Event()
        self._base_restart_delay: float = 5.0
        self._started_at: float = 0.0  # monotonic time of last start_stream()
        self._first_packet_seen: bool = False  # set True once mediamtx reports a publisher
        # Inbound-flow watchdog state. Tracks mediamtx's per-path
        # bytesReceived counter and the monotonic time it last advanced.
        # When the publisher is present but the counter sits flat while
        # the encoder PID is alive, the encoder is frozen and the publish
        # must be restarted. Process-liveness alone is never proof of
        # work; assert the byte counter advances. `_inbound_bytes_value`
        # of -1 means "no sample yet".
        self._inbound_bytes_value: int = -1
        self._inbound_bytes_changed_at: float = 0.0
        self._video_inbound_bytes_per_s: float = 0.0
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
        # In-process GStreamer air-side pipeline. Mutually exclusive
        # with the legacy encoder + wfb_tee subprocess tree; selected
        # at start_stream() based on the ``use_gst_air_pipeline``
        # config flag. When None, the legacy bash-pipeline path is in
        # force.
        self._air_pipeline: AirPipeline | None = None
        # Optional cloud-relay bridge sidecar: when the GST air pipeline
        # is in use AND cloud_relay_url is set, this one ffmpeg subprocess
        # reads RTP from UDP <cloud_rtp_port> and pushes RTSP to the
        # local mediamtx-air. Lifecycle is tied to the air pipeline.
        # Replaces the legacy 3-subprocess bash chain (ffmpeg + python +
        # ffmpeg) with a single ffmpeg.
        self._air_cloud_bridge_process: asyncio.subprocess.Process | None = None
        self._air_cloud_bridge_stderr_task: asyncio.Task | None = None

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

    async def _pgrep_and_sigkill(self, pattern: str) -> int:
        """Run ``pgrep -f -- <pattern>``, SIGKILL every matched PID
        except our own, and return how many were killed.

        ``--`` forces pgrep to treat the next token as the search
        pattern even when it starts with ``-``. Without it, a pattern
        like ``-i /dev/video0`` makes pgrep parse the leading dash as
        a flag and exit with ``invalid option -- ' '``, producing
        zero matches and a silent false negative.
        """
        try:
            proc = await asyncio.create_subprocess_exec(
                "pgrep", "-f", "--", pattern,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.DEVNULL,
            )
            stdout, _ = await asyncio.wait_for(proc.communicate(), timeout=2.0)
        except (FileNotFoundError, TimeoutError, asyncio.CancelledError):
            return 0
        killed = 0
        for line in stdout.decode("utf-8", errors="replace").splitlines():
            line = line.strip()
            if not line:
                continue
            try:
                pid = int(line)
            except ValueError:
                continue
            # Skip our own python process if it's matched by name.
            if pid == os.getpid():
                continue
            try:
                os.kill(pid, signal.SIGKILL)
                killed += 1
            except (ProcessLookupError, OSError):
                pass
        return killed

    async def _kill_orphan_encoder_ffmpegs(self, device_path: str) -> None:
        """Kill stray encoder processes holding the camera.

        Two patterns are swept in sequence:

        * ffmpeg encoders holding ``/dev/videoN`` (USB UVC, V4L2).
        * ``rpicam-vid`` (Pi CSI cameras). rpicam doesn't open
          ``/dev/video*`` directly so the V4L2 pattern misses it. The
          tool is purpose-built so a bare-name match is safe.

        Without this sweep, a freshly-spawned encoder opens the camera
        node and immediately bounces with EBUSY because an orphan from
        a prior cycle is still holding the hardware.
        """
        log = _pkg().log
        killed = 0
        killed += await self._pgrep_and_sigkill(f"-i {device_path}")
        killed += await self._pgrep_and_sigkill("rpicam-vid")
        if killed > 0:
            log.warning(
                "encoder_orphans_killed",
                device=device_path,
                count=killed,
            )

    async def _kill_orphan_bridge_ffmpegs(self, rtsp_url: str) -> None:
        """Kill stray ffmpeg processes pushing to our local mediamtx.

        Mirrors the encoder orphan sweep; the bridge ffmpeg is the
        process that publishes the encoded H.264 stream into mediamtx
        via RTSP. A stale one will hold the publisher slot and the new
        bridge will fail to register the path.
        """
        log = _pkg().log
        killed = await self._pgrep_and_sigkill(rtsp_url)
        if killed > 0:
            log.warning(
                "bridge_orphans_killed",
                destination=rtsp_url,
                count=killed,
            )

    async def start_stream(self) -> bool:
        """Start the encoding and streaming pipeline.

        Returns True if the stream started successfully.
        """
        log = _pkg().log
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
        self._persist_camera_state()

        primary = self._camera_mgr.get_primary()
        if not primary:
            log.error("no_primary_camera")
            self._last_start_error = "no_primary_camera"
            self._state = PipelineState.ERROR
            return False

        # Sweep orphan ffmpegs that may be holding the camera or
        # the mediamtx publisher slot before we spawn fresh ones.
        # Without these sweeps an orphan encoder holds /dev/video*
        # with EBUSY (the new encoder bounces immediately) or an
        # orphan bridge holds the mediamtx publisher slot (the new
        # bridge fails to register the path).
        await self._kill_orphan_encoder_ffmpegs(primary.device_path)
        pipe_uri = f"rtsp://localhost:{self._mediamtx.rtsp_port}/main"
        await self._kill_orphan_bridge_ffmpegs(pipe_uri)

        # In-process GStreamer air-side pipeline. Replaces the legacy
        # ffmpeg-encoder + mediamtx-air + ffmpeg-tee + python
        # sei_injector chain with one PyGObject-driven pipeline. Falls
        # back to the legacy bash path if PyGObject or a compatible
        # encoder element is missing so a misconfigured rig still has
        # video.
        if bool(getattr(self._config, "use_gst_air_pipeline", False)):
            ok = await self._start_air_pipeline(primary)
            if ok:
                return True
            log.warning(
                "air_pipeline_unavailable_fallback",
                msg="falling back to legacy bash air pipeline",
            )
            # Reset so the legacy path's idempotency assumptions hold.
            self._state = PipelineState.STARTING

        # Detect encoder
        self._encoder_type = _pkg().detect_encoder_for_camera(primary)
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

        # Start mediamtx for stream output. ``pipe_uri`` was built above
        # for the bridge-orphan sweep; reuse the same value here so the
        # URL the encoder publishes to matches the URL the sweep matched
        # on, and we don't compute it twice.
        cmd = _pkg().build_encoder_command(
            enc_config, primary.device_path, pipe_uri, camera=primary,
        )

        if not cmd:
            log.error("encoder_command_empty")
            self._state = PipelineState.ERROR
            return False

        # When SEI latency injection is enabled, route the encoder
        # output through the Python NAL injector BEFORE it hits
        # mediamtx. This way every downstream consumer (browser WHEP,
        # over-the-air wfb_tx, drone-side LCD tap) gets the same
        # wall-clock timestamp on the same frame — which is what makes
        # browser-side true camera->monitor glass-to-glass measurable.
        # The wfb_tee bash pipeline below stops re-injecting (the
        # stream from mediamtx already carries SEI) so the marker
        # isn't doubled.
        if bool(getattr(self._config.wfb, "sei_latency", False)):
            from ados.services.video.encoder import wrap_with_sei_inject

            cmd = wrap_with_sei_inject(cmd, pipe_uri)
            log.info("sei_inject_upstream_of_mediamtx", encoder=self._encoder_type.value)

        # Configure and start mediamtx
        self._mediamtx.generate_config({"main": "publisher"})
        mtx_ok = await self._mediamtx.start()
        if not mtx_ok:
            log.error(
                "mediamtx_start_failed",
                msg="cannot stream without mediamtx — install mediamtx",
            )
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
                # New session puts every encoder child in its own
                # process group so a graceful teardown can clean them
                # up via os.killpg(); future encoder generations stop
                # accumulating as orphans when the parent goes away.
                start_new_session=True,
            )
            self._encoder_stderr_task = asyncio.create_task(
                self._drain_stderr(self._encoder_process, "encoder")
            )
            self._state = PipelineState.RUNNING
            self._started_at = time.monotonic()
            self._first_packet_seen = False
            # Reset the inbound-flow window so a fresh publish gets a
            # clean cold-start window before the byte-counter watchdog
            # can trip on it.
            self._inbound_bytes_value = -1
            self._inbound_bytes_changed_at = time.monotonic()
            self._video_inbound_bytes_per_s = 0.0
            self._last_start_error = None
            log.info(
                "pipeline_started",
                encoder=self._encoder_type.value,
                camera=primary.name,
            )
            # Best-effort radio fan-out. Failure here does not fail the
            # pipeline; local mediamtx and cloud push still work, only
            # the radio link goes dark.
            await self.start_wfb_tee()
            # Headless SEI tap on the local mediamtx RTSP feed. Only
            # spawn when wfb.sei_latency is enabled (the markers are
            # what we read). Decoupled from the OLED service so it
            # works on drones without an LCD attached.
            if bool(getattr(self._config.wfb, "sei_latency", False)):
                try:
                    from ados.services.video.sei_tap import HeadlessSeiTap

                    self._sei_tap = HeadlessSeiTap(rtsp_url=pipe_uri)
                    await self._sei_tap.start()
                except Exception as exc:  # noqa: BLE001
                    log.warning("headless_sei_tap_spawn_failed", error=str(exc))
                    self._sei_tap = None
            return True
        except FileNotFoundError:
            log.error("encoder_binary_not_found", encoder=self._encoder_type.value)
            await self._teardown_after_partial_start()
            return False
        except Exception as exc:
            log.error("encoder_start_failed", error=str(exc), exc_info=True)
            await self._teardown_after_partial_start()
            return False

    async def _start_air_pipeline(self, primary) -> bool:
        """Start the in-process GStreamer air pipeline.

        Mediamtx-air is only spawned when ``cloud_relay_url`` is set —
        on a bench / LAN rig the GStreamer pipeline writes RTP straight
        to wfb_tx's UDP 5600 with no RTSP intermediate. When cloud
        relay is enabled, mediamtx-air ingests via a single ffmpeg
        sidecar that bridges UDP 8000 RTP → RTSP push, and republishes
        as RTSP/WHEP.

        Returns False (without setting state to ERROR) when PyGObject
        or a compatible encoder is missing, so the caller can fall
        back to the legacy bash pipeline cleanly.
        """
        log = _pkg().log
        cloud_url = getattr(self._config, "cloud_relay_url", "") or ""
        cloud_enabled = bool(cloud_url)

        # Mediamtx-air only when the cloud branch is in play. The new
        # pipeline doesn't need a local RTSP intermediate for the wfb
        # path; the udpsink goes straight to 127.0.0.1:5600 for wfb_tx.
        if cloud_enabled:
            self._mediamtx.generate_config({"main": "publisher"})
            mtx_ok = await self._mediamtx.start()
            if not mtx_ok:
                log.warning(
                    "air_pipeline_mediamtx_start_failed",
                    msg="cloud relay branch will be inert",
                )
                cloud_enabled = False

        board = detect_board()
        try:
            self._air_pipeline = AirPipeline(
                video_config=self._config,
                camera=primary,
                board_soc=board.soc,
                board_hw_codecs=board.hw_video_codecs,
                cloud_relay_enabled=cloud_enabled,
                sei_latency_enabled=bool(
                    getattr(self._config.wfb, "sei_latency", False)
                ),
            )
            await self._air_pipeline.start()
        except AirPipelineUnavailable as exc:
            log.warning(
                "air_pipeline_unavailable",
                error=str(exc),
            )
            self._air_pipeline = None
            # Tear down mediamtx if we spawned it; the legacy path will
            # re-spawn it shortly with its own config.
            if cloud_enabled:
                try:
                    await self._mediamtx.stop()
                except Exception as stop_exc:  # noqa: BLE001
                    log.debug(
                        "air_pipeline_mediamtx_stop_after_fail",
                        error=str(stop_exc),
                    )
            return False
        except Exception as exc:  # noqa: BLE001
            log.error(
                "air_pipeline_start_failed",
                error=str(exc),
                exc_info=True,
            )
            self._air_pipeline = None
            if cloud_enabled:
                try:
                    await self._mediamtx.stop()
                except Exception as stop_exc:  # noqa: BLE001
                    log.debug(
                        "air_pipeline_mediamtx_stop_after_fail",
                        error=str(stop_exc),
                    )
            self._state = PipelineState.ERROR
            return False

        # Stamp encoder_type so get_status() still surfaces a useful
        # value to the GCS even though no encoder ffmpeg subprocess
        # exists.
        chosen = self._air_pipeline.stats().get("encoder_name") or ""
        try:
            self._encoder_type = EncoderType(chosen)
        except (ValueError, KeyError):
            self._encoder_type = None

        # Spawn the cloud-bridge ffmpeg sidecar when cloud relay is on
        # and mediamtx-air is up. One subprocess vs the legacy 3-stage
        # bash chain. The pipeline's tee element is already emitting
        # RTP at UDP <cloud_rtp_port> — we just need to republish it as
        # RTSP into mediamtx-air for browser WHEP.
        if cloud_enabled:
            await self._start_air_cloud_bridge()

        self._state = PipelineState.RUNNING
        self._started_at = time.monotonic()
        self._first_packet_seen = True  # in-process pipeline; no probe race
        self._last_start_error = None
        log.info(
            "air_pipeline_started",
            camera=primary.name,
            encoder=chosen,
            cloud_branch=cloud_enabled,
        )
        return True

    async def _start_air_cloud_bridge(self) -> bool:
        """Bridge GStreamer's UDP RTP output to mediamtx-air as RTSP push.

        One ffmpeg subprocess reading ``rtp://127.0.0.1:<cloud_rtp_port>``
        and pushing ``rtsp://localhost:8554/main``. Replaces the legacy
        3-subprocess bash chain in the cloud-on path; the bench-only
        path skips this entirely.
        """
        log = _pkg().log
        if (
            self._air_cloud_bridge_process is not None
            and self._air_cloud_bridge_process.returncode is None
        ):
            return True
        cloud_port = int(getattr(self._config, "cloud_rtp_port", 8000))
        local_rtsp = f"rtsp://localhost:{self._mediamtx.rtsp_port}/main"
        # The input ffmpeg needs an SDP to know the codec; we describe
        # H.264/payload-96 inline via the ``-protocol_whitelist`` +
        # ``-i`` form. Simpler than writing a real SDP file to disk.
        sdp_inline = (
            f"v=0\\n"
            f"o=- 0 0 IN IP4 127.0.0.1\\n"
            f"s=ados\\n"
            f"c=IN IP4 127.0.0.1\\n"
            f"t=0 0\\n"
            f"m=video {cloud_port} RTP/AVP 96\\n"
            f"a=rtpmap:96 H264/90000\\n"
        )
        try:
            # Write SDP to /run/ados so the ffmpeg can read it; cheaper
            # than coordinating stdin.
            sdp_path = Path("/run/ados/air-pipeline-cloud.sdp")
            try:
                sdp_path.parent.mkdir(parents=True, exist_ok=True)
            except OSError:
                pass
            try:
                sdp_path.write_text(sdp_inline.replace("\\n", "\n"))
            except OSError as exc:
                log.warning(
                    "air_cloud_bridge_sdp_write_failed", error=str(exc),
                )
                return False
            # `-probesize 5M -analyzeduration 5M` mirror the ground
            # sidecar (mediamtx_manager.py): the SDP we read carries
            # only the encoding name + clock rate, codec config
            # arrives inline in the first IDR. Bench validation
            # showed 1M/1s is too tight — mid-GOP P-frame packets
            # arrive first and ffmpeg throws decode_slice_header /
            # unspecified-size before an IDR lands. 5M/5s is the
            # proven conservative value. NB: still no `-max_delay
            # 0` here — same codec-discovery hazard documented at
            # the bash branch comment above and in the ground
            # sidecar.
            self._air_cloud_bridge_process = await asyncio.create_subprocess_exec(
                "ffmpeg",
                "-protocol_whitelist", "file,udp,rtp",
                "-fflags", "nobuffer",
                "-flags", "low_delay",
                "-probesize", "5M",
                "-analyzeduration", "5M",
                "-i", str(sdp_path),
                "-c:v", "copy",
                "-f", "rtsp",
                "-rtsp_transport", "tcp",
                "-muxdelay", "0",
                "-muxpreload", "0",
                "-flush_packets", "1",
                local_rtsp,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.PIPE,
                start_new_session=True,
            )
            self._air_cloud_bridge_stderr_task = asyncio.create_task(
                self._drain_stderr(
                    self._air_cloud_bridge_process, "air_cloud_bridge"
                )
            )
            log.info(
                "air_cloud_bridge_started",
                pid=self._air_cloud_bridge_process.pid,
                cloud_rtp_port=cloud_port,
                destination=local_rtsp,
            )
            return True
        except FileNotFoundError:
            log.error("air_cloud_bridge_ffmpeg_not_found")
            return False
        except Exception as exc:  # noqa: BLE001
            log.error("air_cloud_bridge_start_failed", error=str(exc))
            return False

    async def _stop_air_cloud_bridge(self) -> None:
        """Stop the cloud-bridge ffmpeg sidecar via process-group SIGTERM."""
        log = _pkg().log
        if self._air_cloud_bridge_stderr_task is not None:
            self._air_cloud_bridge_stderr_task.cancel()
            self._air_cloud_bridge_stderr_task = None
        proc = self._air_cloud_bridge_process
        if proc is not None and proc.returncode is None:
            pgid: int | None = None
            try:
                pgid = os.getpgid(proc.pid)
            except (ProcessLookupError, OSError):
                pgid = None
            try:
                if pgid is not None:
                    os.killpg(pgid, signal.SIGTERM)
                else:
                    proc.terminate()
            except (ProcessLookupError, OSError):
                pass
            try:
                await asyncio.wait_for(proc.wait(), timeout=5.0)
            except (TimeoutError, asyncio.CancelledError):
                try:
                    if pgid is not None:
                        os.killpg(pgid, signal.SIGKILL)
                    else:
                        proc.kill()
                except (ProcessLookupError, OSError):
                    pass
            log.info("air_cloud_bridge_stopped", pgid=pgid)
        self._air_cloud_bridge_process = None

    async def _teardown_after_partial_start(self) -> None:
        """Roll back partial start. Stops any process spawned after mediamtx.start()."""
        log = _pkg().log
        # Sweep the wfb tee first; it depends on local RTSP being up.
        await self.stop_wfb_tee()
        if self._sei_tap is not None:
            try:
                await self._sei_tap.stop()
            except Exception:  # noqa: BLE001
                pass
            self._sei_tap = None
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
        log = _pkg().log
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
        log = _pkg().log
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
            # ``start_new_session=True`` puts the cloud-push ffmpeg in
            # its own process group so the supervisor (or a teardown
            # path) can clean it up cleanly without orphaning it on
            # parent death. Mirrors the encoder + air_cloud_bridge
            # spawns above.
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
                start_new_session=True,
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
        log = _pkg().log
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

    async def set_video_bitrate(self, kbps: int) -> bool:
        """Apply a new encoder bitrate via stop+start.

        libx264 / mpph264enc / rpicam-vid don't expose a hot
        bitrate-reload knob across the stack, so the only correct
        application is to tear the pipeline down and bring it
        back up with the updated bitrate in self._config. Total
        blackout is ~1-2 s, absorbed by the GCS WHEP playout
        buffer and the LCD's last-frame-hold. Refuses values
        outside a sensible 0.5-12 Mbps band so a buggy controller
        cannot drive the link to silence or overrun the FEC
        budget on the radio side.

        Returns True on a clean restart. False when the new
        bitrate failed validation or the restart did not come
        back healthy; in the failure case the next supervisor
        tick will retry on its own.
        """
        log = _pkg().log
        if not 500 <= kbps <= 12000:
            log.warning("set_video_bitrate_out_of_range", kbps=kbps)
            return False
        if self._state != PipelineState.RUNNING:
            # Pipeline isn't up. Persist the new value so the
            # next start_stream picks it up; no restart required.
            self._config.camera.bitrate_kbps = kbps
            log.info("set_video_bitrate_pending_start", kbps=kbps)
            return True
        old = self._config.camera.bitrate_kbps
        log.info("set_video_bitrate_applying", old=old, new=kbps)
        self._config.camera.bitrate_kbps = kbps
        try:
            await self.stop_stream()
        except Exception as exc:  # noqa: BLE001
            log.warning("set_video_bitrate_stop_failed", error=str(exc))
        ok = await self.start_stream()
        if not ok:
            log.warning("set_video_bitrate_restart_failed", kbps=kbps)
            # Roll the config back so the next supervisor restart
            # picks up a value that was already proven to start.
            self._config.camera.bitrate_kbps = old
            return False
        log.info("set_video_bitrate_applied", kbps=kbps)
        return True

    async def stop_stream(self) -> None:
        """Stop the encoding pipeline and mediamtx."""
        log = _pkg().log
        log.info("stop_stream_begin")
        # In-process GStreamer pipeline. Idempotent stop; the legacy
        # bash teardown below is a no-op when the air pipeline owns
        # the stream.
        if self._air_pipeline is not None:
            try:
                await self._air_pipeline.stop()
            except Exception as exc:  # noqa: BLE001
                log.warning("air_pipeline_stop_failed", error=str(exc))
            self._air_pipeline = None
        await self._stop_air_cloud_bridge()
        await self.stop_wfb_tee()
        await self.stop_cloud_push()
        # Headless SEI sample reader; idempotent stop().
        if self._sei_tap is not None:
            try:
                await self._sei_tap.stop()
            except Exception as exc:  # noqa: BLE001
                log.warning("headless_sei_tap_stop_failed", error=str(exc))
            self._sei_tap = None

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
        log = _pkg().log
        # When the in-process GStreamer pipeline owns the stream
        # there is no ffmpeg encoder + mediamtx-air to probe; health
        # is whether the pipeline thread reports a live state. The
        # pipeline's own bus watchdog + tx-byte watchdog handle
        # restart internally per Rule 26 / Rule 37, so the outer
        # restart loop only fires if the pipeline gives up entirely
        # (which by design never happens — there is no give-up).
        if self._air_pipeline is not None:
            return self._air_pipeline.is_running()
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
        # Inbound-flow watchdog: the publisher can be present and `ready`
        # while the encoder has frozen and no bytes are crossing. Assert
        # mediamtx's per-path bytesReceived counter keeps advancing so a
        # frozen-but-alive encoder is restarted — process-liveness alone
        # is never proof of work.
        if not await self._check_inbound_flow_healthy():
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
        log = _pkg().log
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

    async def _read_mediamtx_bytes_received(self) -> int | None:
        """Read the bytesReceived counter for the encoder's named path.

        Looks the path up by its known name (``_MEDIAMTX_MAIN_PATH``) via
        ``/v3/paths/get/<name>`` rather than assuming it is the first item
        in the list — a stray republish path or a future second stream
        would otherwise shift the index. Returns the cumulative inbound
        byte count, or ``None`` when the API is unreachable / the path is
        absent / the field is missing. This is the authoritative "data is
        actually arriving from the encoder" signal, distinct from the
        ``ready`` flag (which can stay true on a publisher whose byte flow
        has frozen).
        """
        try:
            client = await self._get_mediamtx_client()
            resp = await client.get(
                f"/v3/paths/get/{_MEDIAMTX_MAIN_PATH}"
            )
            if resp.status_code != 200:
                return None
            data = resp.json()
            if not isinstance(data, dict):
                return None
            value = data.get("bytesReceived")
            if isinstance(value, int) and value >= 0:
                return value
            return None
        except Exception:
            return None

    async def _check_inbound_flow_healthy(self) -> bool:
        """Verify inbound bytes are still advancing through mediamtx.

        Returns True while the per-path bytesReceived counter keeps
        climbing (or while the publisher is still in its cold-start
        window). Returns False when the counter sits flat for
        _INBOUND_FLOW_STALL_SECONDS while the encoder PID is alive — the
        encoder has frozen mid-stream and the publish must be restarted.
        Falls back to "healthy" (returns True) when the counter cannot be
        read so a transient API hiccup never forces a needless restart;
        the existing path-ready probe still covers the dead-publisher
        case.
        """
        log = _pkg().log
        # Only meaningful once a publisher exists and the encoder is the
        # legacy subprocess (the in-process pipeline runs its own bus +
        # tx-byte watchdog).
        if self._air_pipeline is not None:
            return True
        if self._encoder_process is None:
            return True
        if not self._first_packet_seen:
            return True
        current = await self._read_mediamtx_bytes_received()
        if current is None:
            return True
        now = time.monotonic()
        interval = max(_HEALTH_CHECK_INTERVAL, 0.001)
        if self._inbound_bytes_value < 0:
            self._inbound_bytes_value = current
            self._inbound_bytes_changed_at = now
            return True
        if current > self._inbound_bytes_value:
            delta = current - self._inbound_bytes_value
            elapsed = max(now - self._inbound_bytes_changed_at, interval)
            self._video_inbound_bytes_per_s = delta / elapsed
            self._inbound_bytes_value = current
            self._inbound_bytes_changed_at = now
            return True
        stalled_for = now - self._inbound_bytes_changed_at
        if stalled_for < _INBOUND_FLOW_STALL_SECONDS:
            return True
        self._video_inbound_bytes_per_s = 0.0
        log.warning(
            "video_inbound_flow_stalled",
            bytes_received=current,
            stalled_seconds=round(stalled_for, 1),
            threshold_seconds=_INBOUND_FLOW_STALL_SECONDS,
            note=(
                "encoder PID alive and publisher present but mediamtx "
                "bytesReceived flat; restarting publish"
            ),
        )
        return False

    def video_inbound_bytes_per_s(self) -> float:
        """Most recent inbound video throughput in bytes/s (observability)."""
        return round(self._video_inbound_bytes_per_s, 1)

    async def _check_cloud_push_health(self) -> bool:
        """Check if the cloud push subprocess is still running.

        Returns True if healthy or if cloud push is not configured.
        Returns False only when the process has died unexpectedly.
        """
        log = _pkg().log
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

    def _persist_camera_state(self) -> None:
        """Write a cross-process snapshot of camera discovery state.

        The heartbeat builder in ados-supervisor / ados-api reads
        ``/run/ados/camera-state.json`` to surface a 'Camera Missing'
        pill on the GCS drone card without needing direct access to
        the ados-video process. Best-effort; OSErrors are logged at
        debug and discarded.
        """
        try:
            import json as _json
            from ados.core.paths import CAMERA_STATE_JSON
            primary = self._camera_mgr.get_primary()
            cameras = self._camera_mgr.to_dict()
            if isinstance(cameras, dict):
                total = len(cameras.get("cameras", []) or [])
            else:
                total = 0
            if primary is not None:
                state = "ready"
                primary_path = getattr(primary, "device_path", None)
                primary_name = getattr(primary, "name", None) or getattr(
                    primary, "display_name", None
                )
            else:
                state = "missing"
                primary_path = None
                primary_name = None
            payload = {
                "state": state,
                "primary_path": primary_path,
                "primary_name": primary_name,
                "total_cameras": total,
                "updated_at_unix": time.time(),
            }
            tmp = CAMERA_STATE_JSON.with_suffix(".tmp")
            tmp.parent.mkdir(parents=True, exist_ok=True)
            tmp.write_text(_json.dumps(payload))
            tmp.replace(CAMERA_STATE_JSON)
        except (OSError, Exception) as exc:
            _pkg().log.debug("camera_state_persist_failed", error=str(exc))

    async def _sleep_or_wake_on_camera(self, timeout: float) -> None:
        """Sleep up to ``timeout`` seconds, or wake early if a fresh
        /dev/video* node fires the udev SIGUSR1 to ados-video.

        For non-camera errors (where the camera-plugged event is
        irrelevant) the behaviour collapses to a plain asyncio.sleep,
        because the event is never set by anything else.
        """
        if timeout <= 0:
            return
        try:
            await asyncio.wait_for(
                self._camera_plugged_event.wait(), timeout=timeout
            )
            # Drain the event so the next backoff cycle requires a
            # fresh hotplug to short-circuit.
            self._camera_plugged_event.clear()
        except asyncio.TimeoutError:
            pass

    async def run(self) -> None:
        """Main service loop — monitors pipeline health and restarts on failure.

        On cancellation, ensures the encoder subprocess is terminated and not
        orphaned (A-07).
        """
        log = _pkg().log
        log.info("video_pipeline_service_start")

        # Wire SIGUSR1 to the camera-plugged event so the udev rule at
        # /etc/udev/rules.d/50-ados-uvc-no-autosuspend.rules can wake
        # the no_primary_camera backoff sleep immediately on hotplug.
        # Best-effort — non-Linux loops and re-entries fall back to
        # the legacy timeout-only behaviour.
        try:
            asyncio.get_running_loop().add_signal_handler(
                signal.SIGUSR1, self._camera_plugged_event.set
            )
        except (RuntimeError, NotImplementedError, ValueError):
            pass

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
                        # Refresh /run/ados/camera-state.json so the
                        # heartbeat builder doesn't treat a long-lived
                        # healthy stream as a stale snapshot and drop
                        # the GCS Camera Missing pill incorrectly.
                        self._persist_camera_state()
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
                        # tearing down the rest of the pipeline. Per Rule 26
                        # there is no give-up cap — video must keep retrying
                        # forever. Snappy 5 s backoff ceiling so recovery is
                        # fast when the upstream comes back. The previous
                        # 10-attempt circuit breaker meant a transient
                        # 30-second-window of failures (e.g., during install
                        # or a config reload race) left the LCD frozen until
                        # a manual `systemctl restart` cleared the counter.
                        self._wfb_tee_restart_count += 1
                        delay = min(
                            self._base_restart_delay * (2 ** (self._wfb_tee_restart_count - 1)),
                            5.0,
                        )
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
                    # Pick the backoff cap by error class. A transient
                    # "no_primary_camera" (USB hotplug, cable jiggle, bus
                    # reset) resolves in seconds; polling at 30 s lets the
                    # pipeline catch the re-enumeration at human-attention
                    # cadence. A real driver wedge keeps the 5-minute cap.
                    if self._last_start_error == "no_primary_camera":
                        cap = self._max_restart_delay_no_camera
                    else:
                        cap = self._max_restart_delay
                    delay = min(
                        self._base_restart_delay * (2 ** (self._restart_count - 1)),
                        cap,
                    )
                    if self._restart_count >= 10:
                        log.warning(
                            "pipeline_retry_backoff",
                            msg="10 consecutive failures, backing off",
                            attempts=self._restart_count,
                            backoff_secs=cap,
                            last_error=self._last_start_error,
                        )
                        await self._sleep_or_wake_on_camera(cap)
                        self._restart_count = 0
                        continue
                    log.info(
                        "pipeline_retry_from_error",
                        attempt=self._restart_count,
                        backoff_secs=delay,
                        last_error=self._last_start_error,
                    )
                    await self._sleep_or_wake_on_camera(
                        max(0, delay - _HEALTH_CHECK_INTERVAL)
                    )
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
        air_pipeline_block = None
        if self._air_pipeline is not None:
            air_pipeline_block = self._air_pipeline.stats()
        return {
            "state": self._state.value,
            "encoder": self._encoder_type.value if self._encoder_type else None,
            "cameras": self._camera_mgr.to_dict(),
            "recorder": self._recorder.to_dict(),
            "mediamtx": self._mediamtx.to_dict(),
            "cloud_push": cloud_push,
            "wfb_tee": wfb_tee,
            "air_pipeline": air_pipeline_block,
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
        log = _pkg().log
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
        log = _pkg().log
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
            self._last_start_error = "no_primary_camera"
            self._state = PipelineState.ERROR
            return False

        # Honour the same air-pipeline flag the cold-start path
        # honours so a mid-flight camera switch doesn't silently fall
        # back to the legacy bash pipeline. AirPipeline rebuilds with
        # the new camera object inside ``_start_air_pipeline``.
        if bool(getattr(self._config, "use_gst_air_pipeline", False)):
            ok = await self._start_air_pipeline(primary)
            if ok:
                return True
            log.warning(
                "air_pipeline_unavailable_fallback",
                msg="falling back to legacy bash air pipeline after camera switch",
            )
            self._state = PipelineState.STARTING

        self._encoder_type = _pkg().detect_encoder_for_camera(primary)
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
        cmd = _pkg().build_encoder_command(
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
            # Reset the inbound-flow window so a fresh publish gets a
            # clean cold-start window before the byte-counter watchdog
            # can trip on it.
            self._inbound_bytes_value = -1
            self._inbound_bytes_changed_at = time.monotonic()
            self._video_inbound_bytes_per_s = 0.0
            self._last_start_error = None
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
