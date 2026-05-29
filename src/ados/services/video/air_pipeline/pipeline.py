"""``AirPipeline`` — in-process GStreamer pipeline driving wfb_tx.

Mirrors the receiver-side ``LocalVideoTap`` pattern: a dedicated
thread owns its own ``GMainLoop``, the bus watcher fires restart on
error, and async control surfaces dispatch state-change calls via the
bus thread. The receiver's pad probe is the model for our in-pipeline
SEI injection — same UUID, same byte builder reused from
:mod:`ados.services.video.sei_injector` as a library function so there
is no second subprocess.

Watchdogs
---------

* The bus watcher restarts on ``GST_MESSAGE_ERROR`` / unexpected EOS
  with a 1/2/4/5 s capped exponential backoff. Per Rule 26, there is
  no circuit breaker — the air-side pipeline must keep retrying
  indefinitely or the radio link goes dark.
* The TX-byte watchdog polls ``/sys/class/net/<wfb_iface>/statistics/
  tx_bytes`` at 1 Hz and forces a restart when the counter stays flat
  for ≥ 15 s while the pipeline is PLAYING. Per Rule 37, process
  liveness alone is never proof of work — without this watchdog a
  wedged encoder element shows as "alive" while emitting nothing.
"""

from __future__ import annotations

import asyncio
import json
import threading
import time
from typing import TYPE_CHECKING, Any

from ados.core.logging import get_logger
from ados.core.paths import AIR_PIPELINE_STATS_PATH as _STATS_FILE
from ados.services.video.sei_injector import (
    build_sei_nal,
    is_vcl_nal_type,
)

from .errors import AirPipelineUnavailable
from .pipeline_builder import (
    _read_tx_bytes,
    _resolve_wfb_iface,
    build_air_pipeline_string,
)
from .stats import AirPipelineStats

if TYPE_CHECKING:
    from ados.core.config import VideoConfig
    from ados.hal.camera import CameraInfo


log = get_logger("video.air_pipeline")


# How long the pipeline can run silent (no advance in the kernel's
# wlan tx_bytes counter) before the TX-byte watchdog forces a restart.
# 15 s matches the legacy bash-pipeline progress watchdog; experiment
# proved smaller values false-trip during install + reload races where
# wfb_tx is being reconfigured concurrently.
_TX_SILENT_THRESHOLD_S = 15.0
_TX_POLL_INTERVAL_S = 1.0

# Restart backoff ladder. Per Rule 26 there is no give-up cap; the
# ceiling is 5 s so a transient outage recovers fast.
_RESTART_BACKOFF_LADDER_S: tuple[float, ...] = (1.0, 2.0, 4.0, 5.0, 5.0)

# How often the stats publisher thread writes the snapshot to
# ``/run/ados/air-pipeline.json``. The receiver-side ``lcd-video-tap``
# precedent uses 1 Hz; matching here keeps the GCS Hardware tab's
# refresh cadence symmetric across air + ground halves.
_STATS_PUBLISH_INTERVAL_S = 1.0


class AirPipeline:
    """In-process GStreamer pipeline driving the wfb_tx feed.

    All state-change calls are exposed as ``async`` methods so the
    asyncio video service can await them without blocking. The
    pipeline itself runs on a dedicated daemon thread with its own
    ``GMainLoop``; PyGObject callbacks fire on that thread.
    """

    def __init__(
        self,
        *,
        video_config: VideoConfig,
        camera: CameraInfo | None,
        board_soc: str,
        board_hw_codecs: list[str] | None,
        cloud_relay_enabled: bool,
        sei_latency_enabled: bool,
        logger: Any | None = None,
    ) -> None:
        self._config = video_config
        self._camera = camera
        self._board_soc = board_soc
        self._board_hw_codecs = list(board_hw_codecs or [])
        self._cloud_relay_enabled = bool(cloud_relay_enabled)
        self._sei_latency_enabled = bool(sei_latency_enabled)
        self._logger = logger or log

        self._stats = AirPipelineStats()
        self._stats.cloud_branch_open = self._cloud_relay_enabled
        # Restart bookkeeping.
        self._consecutive_restart_failures: int = 0
        # Lazily-bound PyGObject objects. Held as ``Any`` so the type
        # checker doesn't need PyGObject in CI.
        self._Gst: Any | None = None
        self._GLib: Any | None = None
        self._pipeline: Any | None = None
        self._h264parse: Any | None = None
        self._h264parse_probe_id: int | None = None
        self._wfb_sink: Any | None = None
        self._cloud_gate: Any | None = None
        self._cloud_sink: Any | None = None
        self._loop: Any | None = None
        self._thread: threading.Thread | None = None
        self._stop_requested = threading.Event()
        self._lock = threading.Lock()
        self._tx_watchdog_thread: threading.Thread | None = None
        self._stats_publisher_thread: threading.Thread | None = None
        self._tx_last_advance_at: float = 0.0
        self._tx_last_bytes_seen: int | None = None
        self._wfb_iface: str | None = None
        # Public pipeline launch string + chosen metadata. Set in
        # ``start()``; exposed via ``stats()`` so tests / GCS can verify
        # what was actually launched.
        self._pipeline_str: str = ""

    # ── public API ─────────────────────────────────────────────

    async def start(self) -> None:
        """Construct the pipeline and transition to PLAYING.

        Raises :class:`AirPipelineUnavailable` when PyGObject isn't
        importable or no compatible encoder element is available.
        """
        with self._lock:
            if self._pipeline is not None:
                return
            try:
                import gi
            except ImportError as exc:
                raise AirPipelineUnavailable(
                    "python3-gi or gstreamer not installed"
                ) from exc
            try:
                gi.require_version("Gst", "1.0")
                from gi.repository import GLib, Gst
            except (ImportError, ValueError) as exc:
                raise AirPipelineUnavailable(
                    "gstreamer-1.0 typelib not available"
                ) from exc
            if not Gst.is_initialized():
                Gst.init(None)
            self._Gst = Gst
            self._GLib = GLib

            cam = self._config.camera
            pipeline_str, meta = build_air_pipeline_string(
                camera=self._camera,
                soc=self._board_soc,
                hw_video_codecs=self._board_hw_codecs,
                width=cam.width,
                height=cam.height,
                fps=cam.fps,
                bitrate_kbps=cam.bitrate_kbps,
                # Encoder keyframe interval in frames. A 1-second GOP
                # at the camera fps trades a bit of bandwidth for fast
                # late-joiner recovery and resync after a wfb-ng FEC
                # block loss, sized to the GOP budget that suits this
                # class of hardware.
                keyframe_interval=max(1, int(cam.fps)),
                cloud_branch_enabled=self._cloud_relay_enabled,
                cloud_rtp_port=int(self._config.cloud_rtp_port),
                prefer_hw_encoder=bool(self._config.prefer_hw_encoder),
            )
            self._pipeline_str = pipeline_str
            self._stats.camera_source = str(meta.get("camera_source", ""))
            self._stats.encoder_name = str(meta.get("encoder_name", ""))
            self._stats.encoder_hw_accel = bool(meta.get("encoder_hw_accel"))

            self._logger.info(
                "air_pipeline_pipeline_constructed",
                camera=self._stats.camera_source,
                encoder=self._stats.encoder_name,
                hw_accel=self._stats.encoder_hw_accel,
                cloud_branch=self._cloud_relay_enabled,
                sei_enabled=self._sei_latency_enabled,
            )

            try:
                pipeline = Gst.parse_launch(pipeline_str)
            except Exception as exc:  # noqa: BLE001
                raise AirPipelineUnavailable(
                    f"gstreamer pipeline parse failed: {exc}"
                ) from exc

            h264parse = pipeline.get_by_name("h264parse_air")
            wfb_sink = pipeline.get_by_name("wfb_sink")
            cloud_gate = pipeline.get_by_name("cloud_gate")
            cloud_sink = pipeline.get_by_name("cloud_sink")
            if (
                h264parse is None
                or wfb_sink is None
                or cloud_gate is None
                or cloud_sink is None
            ):
                pipeline.set_state(Gst.State.NULL)
                raise AirPipelineUnavailable(
                    "pipeline missing expected named elements "
                    "(h264parse_air / wfb_sink / cloud_gate / cloud_sink)"
                )

            # Pad probe on h264parse src for SEI injection. Mirror of
            # the receiver-side probe in local_tap; the byte builder
            # lives in sei_injector for byte-level symmetry.
            if self._sei_latency_enabled:
                src_pad = h264parse.get_static_pad("src")
                if src_pad is None:
                    self._logger.warning(
                        "air_pipeline_h264parse_no_src_pad",
                        note="SEI markers will not be injected",
                    )
                else:
                    try:
                        probe_mask = Gst.PadProbeType.BUFFER
                        probe_id = src_pad.add_probe(
                            probe_mask, self._on_h264_buffer, None,
                        )
                        self._h264parse_probe_id = probe_id
                        self._logger.info("air_pipeline_sei_probe_installed")
                    except Exception as exc:  # noqa: BLE001
                        self._logger.warning(
                            "air_pipeline_sei_probe_install_failed",
                            error=str(exc),
                        )
            self._h264parse = h264parse
            self._wfb_sink = wfb_sink
            self._cloud_gate = cloud_gate
            self._cloud_sink = cloud_sink

            bus = pipeline.get_bus()
            bus.add_signal_watch()
            bus.connect("message", self._on_bus_message)

            self._pipeline = pipeline
            self._stop_requested.clear()

            ret = pipeline.set_state(Gst.State.PAUSED)
            if ret == Gst.StateChangeReturn.FAILURE:
                pipeline.set_state(Gst.State.NULL)
                self._pipeline = None
                raise AirPipelineUnavailable(
                    "gstreamer pipeline refused to PAUSED"
                )
            self._stats.pipeline_state = "paused"
            self._stats.last_state_change_at = time.monotonic()

            self._thread = threading.Thread(
                target=self._run_loop_thread,
                name="ados-air-pipeline",
                daemon=True,
            )
            self._thread.start()

            # TX-byte counter watchdog (Rule 37) — gates on the wlan
            # interface advancing. The wfb iface is resolved lazily so
            # a config edit after start_stream is picked up on the
            # next watchdog iteration.
            self._wfb_iface = _resolve_wfb_iface()
            self._tx_last_advance_at = time.monotonic()
            self._tx_last_bytes_seen = None
            self._tx_watchdog_thread = threading.Thread(
                target=self._tx_byte_watchdog,
                name="ados-air-pipeline-watchdog",
                daemon=True,
            )
            self._tx_watchdog_thread.start()

            # Stats publisher. Writes _STATS_FILE at 1 Hz for the REST
            # surface + heartbeat enricher to consume.
            self._stats_publisher_thread = threading.Thread(
                target=self._stats_publisher,
                name="ados-air-pipeline-stats",
                daemon=True,
            )
            self._stats_publisher_thread.start()

            self._stats.started_at = time.monotonic()

    async def stop(self) -> None:
        """Tear down the pipeline cleanly."""
        pipeline = self._pipeline
        loop = self._loop
        thread = self._thread
        gst = self._Gst
        if pipeline is None:
            self._reset_stats()
            return
        self._stop_requested.set()
        # Remove SEI probe first so the callback can't fire on a
        # half-torn-down pipeline.
        h264parse = self._h264parse
        probe_id = self._h264parse_probe_id
        if h264parse is not None and probe_id is not None:
            try:
                src_pad = h264parse.get_static_pad("src")
                if src_pad is not None:
                    src_pad.remove_probe(probe_id)
            except Exception as exc:  # noqa: BLE001
                self._logger.debug(
                    "air_pipeline_sei_probe_remove_failed", error=str(exc)
                )
        try:
            if gst is not None:
                pipeline.send_event(gst.Event.new_eos())
        except Exception as exc:  # noqa: BLE001
            self._logger.debug("air_pipeline_eos_failed", error=str(exc))
        await asyncio.sleep(0.1)
        try:
            if gst is not None:
                pipeline.set_state(gst.State.NULL)
        except Exception as exc:  # noqa: BLE001
            self._logger.debug("air_pipeline_null_failed", error=str(exc))
        if loop is not None:
            try:
                loop.quit()
            except Exception as exc:  # noqa: BLE001
                self._logger.debug(
                    "air_pipeline_loop_quit_failed", error=str(exc)
                )
        if thread is not None:
            thread.join(timeout=1.0)
        with self._lock:
            self._pipeline = None
            self._h264parse = None
            self._h264parse_probe_id = None
            self._wfb_sink = None
            self._cloud_gate = None
            self._cloud_sink = None
            self._loop = None
            self._thread = None
            self._stats.pipeline_state = "stopped"
        self._reset_stats()
        self._logger.info("air_pipeline_stopped")

    def stats(self) -> dict[str, Any]:
        """Snapshot of pipeline telemetry."""
        return self._stats.to_dict()

    def is_running(self) -> bool:
        """Cheap liveness check for the supervisor health probe."""
        return self._pipeline is not None and self._stats.pipeline_state in (
            "playing",
            "paused",
        )

    async def set_bitrate(self, kbps: int) -> None:
        """Live-tune the encoder's bitrate.

        Best-effort: silently no-op if the chosen encoder element does
        not expose the property. Restarting the pipeline is the safe
        fallback for an unsupported encoder.
        """
        if self._pipeline is None:
            return
        encoder = self._pipeline.get_by_name("h264enc")  # legacy name
        if encoder is None:
            return
        try:
            encoder.set_property("bitrate", max(64, int(kbps)))
        except Exception as exc:  # noqa: BLE001
            self._logger.debug("air_pipeline_set_bitrate_failed", error=str(exc))

    async def set_cloud_branch(self, *, open_branch: bool) -> None:
        """Open or close the cloud relay branch at runtime."""
        gate = self._cloud_gate
        if gate is None:
            return
        try:
            gate.set_property("drop-buffers", not bool(open_branch))
            self._stats.cloud_branch_open = bool(open_branch)
            self._logger.info(
                "air_pipeline_cloud_branch_toggled",
                open=bool(open_branch),
            )
        except Exception as exc:  # noqa: BLE001
            self._logger.warning(
                "air_pipeline_cloud_branch_toggle_failed", error=str(exc)
            )

    # ── internals ──────────────────────────────────────────────

    def _reset_stats(self) -> None:
        self._stats.encoder_fps = 0.0
        self._stats.encoded_kbps = 0.0
        self._stats.last_buffer_at = None
        self._stats.sei_injected_count = 0
        # Restart + error counters are deliberately preserved so the
        # operator can see history; only the per-run rates reset.

    def _on_h264_buffer(self, _pad: Any, info: Any, _user: Any) -> int:
        """Pad-probe callback. Injects SEI before each VCL slice.

        Runs on the GStreamer streaming thread. The receiver's parser
        is byte-level tolerant; we only need to splice a SEI NAL in
        front of the first VCL slice in each access unit.
        """
        gst = self._Gst
        if gst is None:
            return 1  # Gst.PadProbeReturn.OK
        try:
            buf = info.get_buffer()
        except Exception:  # noqa: BLE001
            return 1
        if buf is None:
            return 1
        success, mapinfo = buf.map(gst.MapFlags.READ)
        if not success:
            return 1
        try:
            data = bytes(mapinfo.data)
        finally:
            buf.unmap(mapinfo)
        if not data:
            return 1
        modified = self._inject_sei_into_au(data)
        if modified is data:
            return 1
        # Replace the buffer's bytes via a fresh buffer carrying the
        # spliced stream. We can't mutate the existing buffer (it may
        # be ref-shared upstream); instead we allocate, copy, and use
        # gst_buffer_replace to swap the payload in place.
        try:
            new_buf = gst.Buffer.new_wrapped(modified)
            # Preserve timestamps so downstream timing is unchanged.
            new_buf.pts = buf.pts
            new_buf.dts = buf.dts
            new_buf.duration = buf.duration
            new_buf.offset = buf.offset
            new_buf.offset_end = buf.offset_end
            # PadProbeInfo offers data assignment on PyGObject 3.x; the
            # exact attribute name shifts across releases. Try the
            # common shapes and fall back to a debug log if none work.
            for attr in ("data", "buffer"):
                if hasattr(info, attr):
                    try:
                        setattr(info, attr, new_buf)
                        self._stats.sei_injected_count += 1
                        return 1
                    except Exception:  # noqa: BLE001
                        continue
            # No mutable slot found — log once per N misses and let the
            # original buffer pass through unchanged.
            self._logger.debug(
                "air_pipeline_sei_probe_info_immutable",
                note="probe info has no writable buffer slot",
            )
        except Exception as exc:  # noqa: BLE001
            self._logger.debug(
                "air_pipeline_sei_splice_failed", error=str(exc)
            )
        return 1

    def _inject_sei_into_au(self, data: bytes) -> bytes:
        """Return ``data`` with a SEI NAL prefixed before the first VCL slice.

        Walks the Annex-B byte stream once, splices a fresh
        :func:`build_sei_nal(time.time_ns())` in front of the first
        VCL slice it sees. Returns the original ``data`` reference
        when no VCL slice is found (SPS/PPS-only buffer, etc.).
        """
        n = len(data)
        if n < 4:
            return data
        i = 0
        while i + 2 < n:
            sc_len = 0
            if (
                i + 4 <= n
                and data[i] == 0
                and data[i + 1] == 0
                and data[i + 2] == 0
                and data[i + 3] == 1
            ):
                sc_len = 4
            elif (
                i + 3 <= n
                and data[i] == 0
                and data[i + 1] == 0
                and data[i + 2] == 1
            ):
                sc_len = 3
            if sc_len == 0:
                i += 1
                continue
            nal_byte_idx = i + sc_len
            if nal_byte_idx >= n:
                break
            nal_byte = data[nal_byte_idx]
            if is_vcl_nal_type(nal_byte):
                sei = build_sei_nal(time.time_ns())
                return data[:i] + sei + data[i:]
            i = nal_byte_idx + 1
        return data

    def _on_bus_message(self, _bus: Any, message: Any) -> bool:
        """Handle ERROR / EOS / WARNING bus posts and trigger restart."""
        gst = self._Gst
        if gst is None:
            return True
        msg_type = message.type
        if msg_type == gst.MessageType.EOS:
            self._logger.info("air_pipeline_eos")
            self._stats.pipeline_state = "eos"
            self._maybe_auto_restart(reason="eos")
        elif msg_type == gst.MessageType.ERROR:
            err, dbg = message.parse_error()
            self._logger.warning(
                "air_pipeline_bus_error",
                error=str(err),
                debug=str(dbg),
            )
            self._stats.pipeline_state = "error"
            self._stats.bus_errors += 1
            self._maybe_auto_restart(reason="bus_error")
        elif msg_type == gst.MessageType.STATE_CHANGED:
            if message.src is self._pipeline:
                _old, new, _pending = message.parse_state_changed()
                try:
                    name = new.value_nick
                except Exception:  # noqa: BLE001
                    name = str(new)
                if name and name != self._stats.pipeline_state:
                    self._stats.pipeline_state = name
                    self._stats.last_state_change_at = time.monotonic()
                    self._logger.info(
                        "air_pipeline_state_changed", state=name
                    )
        elif msg_type == gst.MessageType.WARNING:
            warn, dbg = message.parse_warning()
            self._logger.debug(
                "air_pipeline_bus_warning",
                warning=str(warn),
                debug=str(dbg),
            )
        return True

    def _run_loop_thread(self) -> None:
        glib = self._GLib
        gst = self._Gst
        pipeline = self._pipeline
        if glib is None or gst is None or pipeline is None:
            return
        loop = glib.MainLoop()
        self._loop = loop
        try:
            pipeline.set_state(gst.State.PLAYING)
            self._stats.pipeline_state = "playing"
            self._stats.last_state_change_at = time.monotonic()
            self._logger.info("air_pipeline_started")
            loop.run()
        except Exception as exc:  # noqa: BLE001
            self._logger.warning(
                "air_pipeline_loop_crashed", error=str(exc)
            )
        finally:
            try:
                pipeline.set_state(gst.State.NULL)
            except Exception:  # noqa: BLE001
                pass

    def _maybe_auto_restart(self, *, reason: str) -> None:
        gst = self._Gst
        pipeline = self._pipeline
        if gst is None or pipeline is None or self._stop_requested.is_set():
            return
        self._consecutive_restart_failures += 1
        ladder_idx = min(
            self._consecutive_restart_failures - 1,
            len(_RESTART_BACKOFF_LADDER_S) - 1,
        )
        delay = _RESTART_BACKOFF_LADDER_S[ladder_idx]
        self._logger.info(
            "air_pipeline_restart_scheduled",
            reason=reason,
            delay_s=delay,
            attempt=self._consecutive_restart_failures,
        )
        self._stats.restart_count = max(
            self._stats.restart_count, self._consecutive_restart_failures
        )
        threading.Timer(
            delay,
            self._do_restart,
            kwargs={"reason": reason},
        ).start()

    def _do_restart(self, *, reason: str) -> None:
        gst = self._Gst
        pipeline = self._pipeline
        if gst is None or pipeline is None or self._stop_requested.is_set():
            return
        try:
            pipeline.set_state(gst.State.NULL)
            try:
                pipeline.get_state(1_000_000_000)
            except Exception:  # noqa: BLE001
                pass
            pipeline.set_state(gst.State.PLAYING)
            self._stats.pipeline_state = "playing"
            self._stats.last_state_change_at = time.monotonic()
            self._logger.info("air_pipeline_auto_restart", reason=reason)
            # On a successful PLAYING transition, clear the consecutive
            # counter so the next stretch of healthy frames does not
            # tick into the back of the ladder.
            self._consecutive_restart_failures = 0
        except Exception as exc:  # noqa: BLE001
            self._logger.warning(
                "air_pipeline_restart_failed", error=str(exc)
            )
            self._stats.pipeline_state = "error"

    def _tx_byte_watchdog(self) -> None:
        """Force a restart when the wlan tx_bytes counter stays flat.

        Rule 37: process liveness alone is never proof of work. A
        wedged encoder element keeps the pipeline in PLAYING while
        emitting zero bytes. The kernel's per-interface counter is the
        authoritative ground truth.
        """
        while not self._stop_requested.is_set():
            try:
                time.sleep(_TX_POLL_INTERVAL_S)
            except Exception:  # noqa: BLE001
                return
            if self._stop_requested.is_set():
                return
            if self._stats.pipeline_state != "playing":
                # Reset the silence timer whenever we're not in PLAYING
                # so a paused/idle window doesn't pre-arm a false kick.
                self._tx_last_advance_at = time.monotonic()
                self._tx_last_bytes_seen = None
                continue
            iface = self._wfb_iface
            if iface is None:
                # Re-resolve lazily; config might have landed late.
                iface = _resolve_wfb_iface()
                self._wfb_iface = iface
                if iface is None:
                    continue
            bytes_now = _read_tx_bytes(iface)
            if bytes_now is None:
                continue
            last = self._tx_last_bytes_seen
            if last is None or bytes_now > last:
                self._tx_last_advance_at = time.monotonic()
                self._tx_last_bytes_seen = bytes_now
                self._stats.udp_bytes_out = bytes_now
                self._stats.last_buffer_at = self._tx_last_advance_at
                continue
            silent_for = time.monotonic() - self._tx_last_advance_at
            if silent_for < _TX_SILENT_THRESHOLD_S:
                continue
            self._logger.warning(
                "air_pipeline_tx_silent",
                silent_s=round(silent_for, 1),
                threshold_s=_TX_SILENT_THRESHOLD_S,
                iface=iface,
                note="kernel tx_bytes flat while PLAYING; forcing restart",
            )
            self._stats.tx_silent_kicks += 1
            try:
                self._maybe_auto_restart(reason="tx_silent")
            except Exception as exc:  # noqa: BLE001
                self._logger.warning(
                    "air_pipeline_watchdog_restart_failed", error=str(exc)
                )
            # Re-arm so we don't immediately re-trigger before the
            # restart lands.
            self._tx_last_advance_at = time.monotonic()
            self._tx_last_bytes_seen = None

    def _stats_publisher(self) -> None:
        """Write the stats snapshot to _STATS_FILE at 1 Hz.

        Also feeds the cumulative ``bus_errors`` counter into the
        auto-fallback watcher. When the watcher concludes the
        GStreamer pipeline is misbehaving (>20 bus errors in 60s —
        vendor MPP plugin regression, kernel/driver mismatch, etc.),
        it writes a runtime override at
        ``/run/ados/video-encoder-override.yaml`` that flips
        ``use_gst_air_pipeline`` to False for the next start_stream
        cycle. We don't restart the service from here — the next
        health-check restart in the outer pipeline picks up the
        override.

        ``bus_errors`` is monotonically increasing on the same
        ``AirPipelineStats`` instance across in-process restarts
        (``_do_restart`` resets state on the same pipeline without
        zeroing stats); a process restart re-zeroes the counter AND
        the watcher's "already triggered" flag so the new session
        re-evaluates from scratch.
        """
        from .auto_fallback import AirPipelineHealthWatcher

        run_dir = _STATS_FILE.parent
        try:
            run_dir.mkdir(parents=True, exist_ok=True)
        except OSError:
            # /run/ados is created by tmpfiles.d on a real rig; on a
            # dev host without /run/ados the publisher silently
            # disables itself.
            return
        health_watcher = AirPipelineHealthWatcher()
        while not self._stop_requested.is_set():
            try:
                time.sleep(_STATS_PUBLISH_INTERVAL_S)
            except Exception:  # noqa: BLE001
                return
            if self._stop_requested.is_set():
                return
            try:
                snapshot = self._stats.to_dict()
                snapshot["updated_at_ms"] = int(time.time() * 1000)
                tmp = _STATS_FILE.with_suffix(".tmp")
                tmp.write_text(json.dumps(snapshot))
                tmp.replace(_STATS_FILE)
                # Feed bus_errors AFTER the publish so the file always
                # reflects what we observed even if the watcher trips.
                # Watcher is idempotent — once the override file is
                # written we don't repeat writes inside this process.
                health_watcher.observe(int(snapshot.get("bus_errors", 0)))
                health_watcher.maybe_trigger_fallback()
            except Exception as exc:  # noqa: BLE001
                self._logger.debug(
                    "air_pipeline_stats_publish_failed", error=str(exc)
                )
