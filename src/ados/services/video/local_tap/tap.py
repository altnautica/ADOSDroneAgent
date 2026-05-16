"""``LocalVideoTap`` — async-friendly facade over a GStreamer appsink.

Owns a dedicated ``GMainLoop`` thread so PyGObject callbacks fire off
the asyncio loop. The asyncio render path reads the latest frame from
:class:`_FrameSlot`. Bus errors trigger an unbounded restart with a
2-second floor; per-frame SEI markers feed the wall-clock latency
EWMA the LCD's metrics row surfaces.
"""

from __future__ import annotations

import asyncio
import os
import threading
import time
from pathlib import Path
from typing import Any

from PIL import Image

from ados.core.logging import get_logger

from .frame_slot import _FrameSlot
from .pipeline_string import (
    _detect_soc,
    build_pipeline_string,
    select_decoder,
)
from .sei_parser import parse_sei_latency_ns

log = get_logger("video.local_tap")


# Latency sanity bounds. A negative value means the air-side and
# ground-side wall clocks have drifted relative to each other (NTP
# correction in flight, large drift, or unsynced rigs); a value above
# 5 s implies a stale buffer or a bogus SEI payload.
_LATENCY_MIN_MS = 0.0
_LATENCY_MAX_MS = 5_000.0

# EWMA smoothing factor for both FPS and latency. 0.2 gives a half-life
# of roughly 3 samples — fast enough to track real load swings without
# bouncing on a single dropped frame.
_EWMA_ALPHA = 0.2

# FPS emit cadence. The renderer reads `stats()` at 1 Hz so the value
# is recomputed at the same cadence. We track a frame counter that is
# converted into instantaneous fps on every sample, then EWMA-smoothed.
_FPS_TICK_SECONDS = 1.0

# Phase 11 default video source for the LCD tap: the UDP port the
# WfbRxManager's fan-out emits to (alongside mediamtx-gs's existing
# UDP 5600 ingest port). udpsrc reads directly from this port, skipping
# the mediamtx-gs RTSP server that was the source of the freeze
# cascade (rtspsrc 404 races, caps re-negotiation failures).
#
# Legacy callers (tests and the older rtspsrc bench-debug path) can
# still pass an ``rtsp://...`` URL to LocalVideoTap.__init__; the
# pipeline string builder dispatches on the prefix.
DEFAULT_RTSP_URL = "5605"

# Target geometry for the LCD video region (480 px wide, 176 px tall —
# leaves a 56 px metrics strip below).
DEFAULT_WIDTH = 480
DEFAULT_HEIGHT = 176

# Bus auto-restart policy. Fixed 2-second retry, forever.
#
# The ground station's job is to show video. Recovery latency from a
# transient upstream outage matters far more than the CPU saved by an
# exponential backoff: each restart attempt is two gstreamer state
# transitions (~20 ms of work) so even at 0.5 Hz the cost is well
# under 1% of one Pi 4B core, and the user sees the picture come
# back within ~1 second of upstream recovering.
#
# Why no exponential backoff: the most common bus error is a startup-
# race 404 from rtspsrc against mediamtx-gs (publisher not yet up).
# That clears in 5-30 s on its own. With backoff, the operator could
# end up waiting 30 s for the next probe to land in the recovery
# window. Fixed 2 s means median recovery ≈ 1 s.
#
# Why no cap: per Rule 26, any operator-visible failure that needs
# SSH to clear is a bug. The tap retries indefinitely while the user
# is on the video page; ``on_leave`` cancels the timer.
_RESTART_RETRY_INTERVAL_SECONDS = 2.0

# Reconnect ladder for the RTSP source itself when the pipeline starts
# but never receives a frame. Capped at 30 s so a long encoder outage
# doesn't compound.
_RECONNECT_LADDER_SECONDS: tuple[float, ...] = (1.0, 2.0, 4.0, 8.0, 30.0)

# Frame-arrival watchdog: number of seconds of pipeline_state="playing"
# with no fresh appsink callback before the watchdog forces a restart.
# 10 s gives the pipeline time to absorb a brief decoder stall, queue
# rebuild, or rtspsrc reconnect storm without tripping a false-positive
# restart that itself feeds back into the loop. The original 3 s was
# too aggressive — bench at v0.20.21 showed a ~5500-sample successful
# run interrupted by a 3.3 s silence window, watchdog kicked, and the
# subsequent restart triggered another bus_error in a cascade. The
# operator perception of "stuck" is on a longer timescale (15 s+)
# anyway, so 10 s catches real freezes without the false positives.
_FRAME_SILENCE_THRESHOLD_S = 10.0
# Watchdog poll interval. 1 Hz is fine — we're checking a flag, not
# reading the GStreamer bus.
_FRAME_SILENCE_POLL_S = 1.0


class LocalVideoTapUnavailable(RuntimeError):  # noqa: N818
    """Raised by :meth:`LocalVideoTap.start` when gstreamer is missing.

    Carries a short reason the page can surface to the operator. The LCD
    service catches this exception and renders a fail-soft card instead
    of stopping the service.
    """


class LocalVideoTap:
    """Async-friendly facade over a gstreamer ``appsink`` consumer.

    All state-change calls (``start``, ``stop``, ``pause``, ``resume``)
    are exposed as ``async`` methods so callers can await them without
    blocking the asyncio loop. The pipeline itself runs on a dedicated
    daemon thread with its own ``GMainLoop`` instance; PyGObject's bus
    callbacks fire on that thread, push the latest frame into a
    threadsafe slot, and the asyncio render path reads the slot at
    its own cadence.
    """

    def __init__(
        self,
        *,
        source_url: str = DEFAULT_RTSP_URL,
        width: int = DEFAULT_WIDTH,
        height: int = DEFAULT_HEIGHT,
        fps_cap: int = 15,
        logger: Any | None = None,
    ) -> None:
        self._source_url = source_url
        self._width = width
        self._height = height
        self._fps_cap = max(1, int(fps_cap))
        self._logger = logger or log

        self._frame_holder = _FrameSlot()
        self._frames_decoded: int = 0
        self._frames_dropped: int = 0
        self._first_frame_at: float | None = None
        self._last_frame_at: float | None = None

        # FPS bookkeeping. ``_fps_tick_count`` accumulates new-sample
        # callbacks since the last 1 Hz tick; ``_fps_tick_at`` is the
        # monotonic timestamp of the most recent tick. ``_fps_ewma`` is
        # the smoothed value the renderer reads. All three reset to
        # zero on ``stop()`` so a paused tap does not show stale FPS
        # after a restart.
        self._fps_tick_count: int = 0
        self._fps_tick_at: float | None = None
        self._fps_ewma: float = 0.0

        # Glass-to-glass latency bookkeeping. ``_latency_ewma`` is None
        # until at least one valid SEI marker is observed; once a
        # sample is rejected by the sanity guard we keep the previous
        # smoothed value so a single bogus buffer does not blank the
        # metric.
        self._latency_ewma: float | None = None
        self._latency_last_sample_at: float | None = None
        self._latency_samples: int = 0
        # Counts h264 buffers that arrived without an ADOS SEI marker.
        # Reset on each successful parse; warning logged every 100 to
        # surface a sustained absence (encoder-side flag off, network
        # drop, h264parse alignment misconfig) without flooding logs.
        self._sei_miss_count: int = 0
        # Frame-arrival watchdog state. Records the last
        # ``_last_frame_at`` value the watchdog observed at its
        # previous poll. If pipeline is PLAYING and this stamp hasn't
        # advanced beyond ``_FRAME_SILENCE_THRESHOLD_S``, force restart.
        self._watchdog_thread: threading.Thread | None = None

        self._decoder_type: str | None = None
        self._pipeline_state: str = "idle"
        # Counter only used to walk the backoff ladder. Never used as
        # a give-up gate — the tap retries forever.
        self._consecutive_restart_failures: int = 0

        # Lazily-bound gstreamer / PyGObject objects. Held as Any so the
        # type checker doesn't need PyGObject installed in CI.
        self._Gst: Any | None = None
        self._GLib: Any | None = None
        self._pipeline: Any | None = None
        self._appsink: Any | None = None
        self._h264parse: Any | None = None
        self._h264parse_probe_id: int | None = None
        self._loop: Any | None = None
        self._thread: threading.Thread | None = None
        self._stop_requested = threading.Event()
        self._lock = threading.Lock()

    # ── public API ─────────────────────────────────────────────

    async def start(self) -> None:
        """Construct the pipeline and transition to PLAYING.

        Raises :class:`LocalVideoTapUnavailable` when PyGObject is not
        importable (typical on a rig where ``install.sh`` hasn't run
        yet, or on a dev laptop without ``python3-gi``). Otherwise
        returns once the pipeline is in PAUSED + the loop thread is up;
        PLAYING transition is fired immediately afterwards on the loop
        thread.
        """
        with self._lock:
            if self._pipeline is not None:
                return
            try:
                import gi
            except ImportError as exc:
                raise LocalVideoTapUnavailable(
                    "python3-gi or gstreamer not installed"
                ) from exc
            try:
                gi.require_version("Gst", "1.0")
                from gi.repository import GLib, Gst
            except (ImportError, ValueError) as exc:
                raise LocalVideoTapUnavailable(
                    "gstreamer-1.0 typelib not available"
                ) from exc
            if not Gst.is_initialized():
                Gst.init(None)
            self._Gst = Gst
            self._GLib = GLib

            decoder = select_decoder(_detect_soc())
            self._decoder_type = decoder
            # rtspsrc internal jitter buffer. We tried 5 ms and 30 ms
            # but rtspsrc's internal loop on Pi 4B + GStreamer 1.22
            # refuses to negotiate at those tight constraints and
            # throws `streaming stopped, reason not-linked` within
            # seconds of the first frame, causing a restart loop.
            # The known-good baseline is 50 ms (hw decoder) / 100 ms
            # (avdec_h264). Pushing past this floor requires bypassing
            # rtspsrc entirely (read UDP 5600 directly via udpsrc) —
            # that's a separate refactor; deferred until v3 of the
            # LCD path.
            latency_ms = 50 if decoder != "avdec_h264" else 100
            pipeline_str = build_pipeline_string(
                source_url=self._source_url,
                decoder=decoder,
                width=self._width,
                height=self._height,
                latency_ms=latency_ms,
                fps_cap=self._fps_cap,
            )
            self._logger.info(
                "local_tap_pipeline_constructed",
                decoder=decoder,
                source_url=self._source_url,
                width=self._width,
                height=self._height,
                fps_cap=self._fps_cap,
            )
            try:
                pipeline = Gst.parse_launch(pipeline_str)
            except Exception as exc:  # noqa: BLE001
                raise LocalVideoTapUnavailable(
                    f"gstreamer pipeline parse failed: {exc}"
                ) from exc
            appsink = pipeline.get_by_name("tap")
            if appsink is None:
                raise LocalVideoTapUnavailable(
                    "appsink element 'tap' missing from pipeline"
                )
            appsink.connect("new-sample", self._on_new_sample)
            bus = pipeline.get_bus()
            bus.add_signal_watch()
            bus.connect("message", self._on_bus_message)

            # Best-effort pad probe on the h264parse src pad for SEI
            # latency markers. The element is unnamed in the pipeline
            # string; iterate the pipeline to find it. If it cannot be
            # located the rest of the tap still works — latency just
            # stays unset.
            # Prefer get_by_name for the named h264parse element; the
            # legacy iterator walk is kept as a fallback for any path
            # that constructs a custom pipeline string without the name.
            h264parse = pipeline.get_by_name("h264parse_tap")
            if h264parse is None:
                h264parse = self._find_h264parse(pipeline)
            if h264parse is None:
                # Surface this as a warning, not a debug whisper — without
                # the probe, latency stays blank on the LCD and there's no
                # other signal that something is wrong.
                self._logger.warning(
                    "local_tap_h264parse_not_found",
                    note="latency markers will not be parsed",
                )
            else:
                src_pad = h264parse.get_static_pad("src")
                if src_pad is None:
                    self._logger.warning(
                        "local_tap_h264parse_no_src_pad",
                    )
                else:
                    try:
                        probe_mask = Gst.PadProbeType.BUFFER
                        probe_id = src_pad.add_probe(
                            probe_mask, self._on_h264_buffer, None,
                        )
                        self._h264parse = h264parse
                        self._h264parse_probe_id = probe_id
                        self._logger.info(
                            "local_tap_h264_probe_installed",
                        )
                    except Exception as exc:  # noqa: BLE001
                        self._logger.warning(
                            "local_tap_h264_probe_install_failed",
                            error=str(exc),
                        )

            self._pipeline = pipeline
            self._appsink = appsink
            self._stop_requested.clear()

            # Move to PAUSED here so the source can preroll. The loop
            # thread bumps to PLAYING right after it starts. Splitting
            # the transition this way lets a slow RTSP DESCRIBE (cold
            # encoder, wfb-tx still warming up) finish before the
            # operator's first render tick.
            ret = pipeline.set_state(Gst.State.PAUSED)
            if ret == Gst.StateChangeReturn.FAILURE:
                pipeline.set_state(Gst.State.NULL)
                self._pipeline = None
                self._appsink = None
                raise LocalVideoTapUnavailable(
                    "gstreamer pipeline refused to PAUSED"
                )
            self._pipeline_state = "paused"

            self._thread = threading.Thread(
                target=self._run_loop_thread,
                name="ados-local-tap",
                daemon=True,
            )
            self._thread.start()
            # Frame-arrival watchdog. The bus-error path catches a gst
            # pipeline crash but a subtler fault is "PLAYING but no
            # frames flowing" — the appsink never fires, the FrameSlot
            # keeps serving the same old frame, and the LCD shows a
            # frozen image until something else trips the bus. Per
            # operating Rule 37, process-liveness is never proof of
            # work; check the actual frame-arrival counter and kick a
            # restart when it goes flat.
            self._watchdog_thread = threading.Thread(
                target=self._frame_silence_watchdog,
                name="ados-local-tap-watchdog",
                daemon=True,
            )
            self._watchdog_thread.start()

    async def stop(self) -> None:
        """Tear down the pipeline cleanly.

        Posts EOS first so trailing buffers flush, waits up to 1 s for
        the bus to acknowledge, then sets state NULL and joins the
        loop thread. Idempotent — repeated calls after stop are no-ops.
        """
        pipeline = self._pipeline
        loop = self._loop
        thread = self._thread
        gst = self._Gst
        if pipeline is None:
            self._reset_stats()
            return
        self._stop_requested.set()
        # Remove the h264parse probe before any state change so the
        # callback can no longer fire on a half-torn-down pipeline.
        h264parse = self._h264parse
        probe_id = self._h264parse_probe_id
        if h264parse is not None and probe_id is not None:
            try:
                src_pad = h264parse.get_static_pad("src")
                if src_pad is not None:
                    src_pad.remove_probe(probe_id)
            except Exception as exc:  # noqa: BLE001
                self._logger.debug(
                    "local_tap_h264_probe_remove_failed",
                    error=str(exc),
                )
        try:
            if gst is not None:
                pipeline.send_event(gst.Event.new_eos())
        except Exception as exc:  # noqa: BLE001
            self._logger.debug("local_tap_eos_failed", error=str(exc))
        # Spin briefly to let EOS land. The loop thread quits on EOS,
        # so wait_for it instead of an arbitrary sleep.
        await asyncio.sleep(0.1)
        try:
            pipeline.set_state(gst.State.NULL) if gst is not None else None
        except Exception as exc:  # noqa: BLE001
            self._logger.debug("local_tap_null_failed", error=str(exc))
        if loop is not None:
            try:
                loop.quit()
            except Exception as exc:  # noqa: BLE001
                self._logger.debug("local_tap_loop_quit_failed", error=str(exc))
        if thread is not None:
            thread.join(timeout=1.0)
        with self._lock:
            self._pipeline = None
            self._appsink = None
            self._h264parse = None
            self._h264parse_probe_id = None
            self._loop = None
            self._thread = None
            self._pipeline_state = "stopped"
        self._reset_stats()
        self._logger.info("local_tap_stopped")

    def _reset_stats(self) -> None:
        """Zero out FPS / latency state on stop so a restart starts clean."""
        self._fps_tick_count = 0
        self._fps_tick_at = None
        self._fps_ewma = 0.0
        self._latency_ewma = None
        self._latency_last_sample_at = None
        self._latency_samples = 0
        self._sei_miss_count = 0
        # Clear the frame-arrival stamp so the silence watchdog doesn't
        # immediately fire on the new pipeline run before any frame has
        # had a chance to arrive (cold start can take 1-2 s).
        self._last_frame_at = None
        self._first_frame_at = None

    async def pause(self) -> None:
        """Transition the pipeline to PAUSED without tearing down."""
        gst = self._Gst
        pipeline = self._pipeline
        if gst is None or pipeline is None:
            return
        try:
            pipeline.set_state(gst.State.PAUSED)
            self._pipeline_state = "paused"
        except Exception as exc:  # noqa: BLE001
            self._logger.debug("local_tap_pause_failed", error=str(exc))

    async def resume(self) -> None:
        """Transition the pipeline back to PLAYING."""
        gst = self._Gst
        pipeline = self._pipeline
        if gst is None or pipeline is None:
            return
        try:
            pipeline.set_state(gst.State.PLAYING)
            self._pipeline_state = "playing"
        except Exception as exc:  # noqa: BLE001
            self._logger.debug("local_tap_resume_failed", error=str(exc))

    def latest_frame(self) -> Image.Image | None:
        """Return the most recent decoded frame or ``None``."""
        return self._frame_holder.get()

    def stats(self) -> dict[str, Any]:
        """Snapshot of decoder telemetry for the metrics strip."""
        first = self._first_frame_at
        ms_since = None
        if first is not None:
            ms_since = int((time.monotonic() - first) * 1000)
        return {
            "decoder_type": self._decoder_type,
            "fps": self._compute_fps(),
            "frames_decoded": self._frames_decoded,
            "frames_dropped": self._frames_dropped,
            "first_frame_at": first,
            "ms_since_first_frame": ms_since,
            "pipeline_state": self._pipeline_state,
            "latency_ms": self._latency_ewma,
            "latency_samples": self._latency_samples,
            "pipeline_latency_ms": self._query_pipeline_latency_ms(),
            "decode_cpu_percent": self._read_decode_cpu_percent(),
        }

    def persist_stats_to_file(
        self,
        path: str | Path = "/run/ados/lcd-latency.json",
    ) -> None:
        """Write the current stats snapshot to a JSON state file.

        Bridges the LocalVideoTap (running in the OLED/UI service
        process) to the API service which serves /api/video/latency.
        Atomic tmpfile+rename so a concurrent reader never sees a
        truncated file. Best-effort: any I/O error is swallowed
        with a debug log; the metric is not critical-path.
        """
        try:
            snapshot = self.stats()
            snapshot["wall_time_unix"] = time.time()
            tmp = Path(str(path)).with_suffix(".tmp")
            import json as _json
            tmp.parent.mkdir(parents=True, exist_ok=True)
            tmp.write_text(_json.dumps(snapshot))
            tmp.replace(Path(str(path)))
        except OSError as exc:
            self._logger.debug("local_tap_persist_failed", error=str(exc))

    # ── internals ──────────────────────────────────────────────

    def _compute_fps(self) -> float:
        """Return the EWMA-smoothed FPS as of the last 1 Hz tick.

        The new-sample callback maintains ``_fps_tick_count`` and the
        per-tick wall clock; this method folds the count into the EWMA
        whenever at least ``_FPS_TICK_SECONDS`` have elapsed since the
        previous tick. Calling order is unimportant — the renderer
        reads at 1 Hz and the new-sample callback bumps the counter
        many times per tick on a 30 fps stream.
        """
        now = time.monotonic()
        last = self._fps_tick_at
        if last is None:
            # First call after start — seed the tick clock without
            # emitting a value so a single early-arriving frame doesn't
            # show as 60 fps.
            self._fps_tick_at = now
            return self._fps_ewma
        elapsed = now - last
        if elapsed < _FPS_TICK_SECONDS:
            return self._fps_ewma
        instant = self._fps_tick_count / elapsed if elapsed > 0 else 0.0
        if self._fps_ewma <= 0:
            self._fps_ewma = instant
        else:
            self._fps_ewma = (
                _EWMA_ALPHA * instant + (1.0 - _EWMA_ALPHA) * self._fps_ewma
            )
        self._fps_tick_count = 0
        self._fps_tick_at = now
        return self._fps_ewma

    @staticmethod
    def _find_h264parse(pipeline: Any) -> Any | None:
        """Locate the ``h264parse`` element inside ``pipeline``.

        Iterates the pipeline's bin children — the element is created
        by ``Gst.parse_launch`` from the unnamed ``! h264parse !`` token
        in the pipeline string.
        """
        try:
            iterator = pipeline.iterate_elements()
        except Exception:  # noqa: BLE001
            return None
        # ``Iterator`` returns one of (Gst.IteratorResult.OK,
        # Gst.IteratorResult.DONE, ...) but we only consume the value
        # so a small loop is enough.
        while True:
            try:
                result = iterator.next()
            except Exception:  # noqa: BLE001
                return None
            # Result is (status, value) on PyGObject 3.x.
            if not isinstance(result, tuple) or len(result) != 2:
                return None
            status, element = result
            # Status 0 == OK, 2 == DONE; bail on anything else.
            try:
                done = int(status) != 0
            except (TypeError, ValueError):
                done = True
            if element is None or done:
                return None
            try:
                factory = element.get_factory()
                name = factory.get_name() if factory is not None else ""
            except Exception:  # noqa: BLE001
                name = ""
            if name == "h264parse":
                return element

    def _on_h264_buffer(self, _pad: Any, info: Any, _user: Any) -> int:
        """Pad-probe callback. Scans the buffer for our SEI marker.

        Runs on the gstreamer streaming thread. Keep it fast — the
        SEI parser early-exits on the first non-matching NAL header
        byte, so even a 1080p I-frame with no SEI is parsed in a few
        microseconds.
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
            stream = bytes(mapinfo.data)
        except Exception:  # noqa: BLE001
            buf.unmap(mapinfo)
            return 1
        try:
            encoded_ns = parse_sei_latency_ns(stream)
        except Exception as exc:  # noqa: BLE001
            self._logger.debug("local_tap_sei_parse_failed", error=str(exc))
            encoded_ns = None
        finally:
            buf.unmap(mapinfo)
        if encoded_ns is None:
            self._sei_miss_count += 1
            # Every 100 missed buffers, surface a single warning so a
            # sustained absence of SEI markers is visible without
            # spamming the journal. Reset the streak counter on the
            # log so the next 100 missed buffers produce another log
            # only if the problem persists.
            if self._sei_miss_count >= 100:
                self._logger.warning(
                    "local_tap_sei_miss_streak",
                    misses=self._sei_miss_count,
                    total_samples=self._latency_samples,
                    note=(
                        "h264 buffers arriving without ADOS SEI markers; "
                        "check video.wfb.sei_latency on the air-side rig"
                    ),
                )
                self._sei_miss_count = 0
            return 1
        # Reset the miss streak on a successful parse so the next log
        # window only fires after a fresh sustained gap.
        self._sei_miss_count = 0
        self._record_latency_sample(encoded_ns)
        return 1

    def _record_latency_sample(self, encoded_ns: int) -> None:
        """Apply sanity guard + EWMA on a fresh latency sample.

        ``encoded_ns`` is the air-side encoder's ``time.time_ns()`` at
        frame-encode time; we subtract from our own ``time.time_ns()``
        at parse time. Both clocks are wall-clock (NTP-synced on the
        LAN), so the delta is the actual elapsed wall time even though
        the encoder ran on a different host. ``time.monotonic_ns()``
        used to live here, but its epoch is per-process so the math
        was nonsense across hosts.
        """
        now_ns = time.time_ns()
        delta_ms = (now_ns - encoded_ns) / 1_000_000.0
        if delta_ms < _LATENCY_MIN_MS or delta_ms > _LATENCY_MAX_MS:
            self._logger.debug(
                "local_tap_latency_rejected",
                delta_ms=delta_ms,
            )
            return
        if self._latency_ewma is None:
            self._latency_ewma = delta_ms
        else:
            self._latency_ewma = (
                _EWMA_ALPHA * delta_ms
                + (1.0 - _EWMA_ALPHA) * self._latency_ewma
            )
        self._latency_last_sample_at = time.monotonic()
        self._latency_samples += 1

    def _query_pipeline_latency_ms(self) -> float | None:
        """Best-effort gstreamer pipeline latency query.

        Posts a ``Gst.Query.new_latency()`` to the pipeline; if the
        upstream element can answer, returns the *minimum* latency in
        milliseconds. Failures (no Gst, no pipeline, query refused)
        return ``None`` so the renderer can show "—".
        """
        gst = self._Gst
        pipeline = self._pipeline
        if gst is None or pipeline is None:
            return None
        try:
            query = gst.Query.new_latency()
            if not pipeline.query(query):
                return None
            _live, min_latency_ns, _max = query.parse_latency()
        except Exception:  # noqa: BLE001
            return None
        try:
            return float(min_latency_ns) / 1_000_000.0
        except (TypeError, ValueError):
            return None

    @staticmethod
    def _read_decode_cpu_percent() -> float | None:
        """Best-effort current-process CPU-percent reading.

        The gstreamer pipeline runs on a daemon thread inside this
        Python process, so the agent's own CPU usage is the closest
        practical proxy for "decode-thread CPU". A more granular
        per-thread reading would require mining ``/proc/self/task/*``
        directly, which is more code than the metric merits at this
        cadence (1 Hz). Returns ``None`` if psutil is missing.
        """
        try:
            import psutil

            return float(psutil.Process(os.getpid()).cpu_percent(interval=0))
        except Exception:  # noqa: BLE001
            return None

    def _run_loop_thread(self) -> None:
        """Owns the ``GMainLoop`` and bumps the pipeline to PLAYING."""
        glib = self._GLib
        gst = self._Gst
        pipeline = self._pipeline
        if glib is None or gst is None or pipeline is None:
            return
        loop = glib.MainLoop()
        self._loop = loop
        try:
            pipeline.set_state(gst.State.PLAYING)
            self._pipeline_state = "playing"
            loop.run()
        except Exception as exc:  # noqa: BLE001
            self._logger.warning("local_tap_loop_crashed", error=str(exc))
        finally:
            try:
                pipeline.set_state(gst.State.NULL)
            except Exception:  # noqa: BLE001
                pass

    def _on_new_sample(self, sink: Any) -> Any:
        """Pull one buffer, copy to RGB, store in the frame slot."""
        gst = self._Gst
        if gst is None:
            return 0  # Gst.FlowReturn.OK
        sample = sink.emit("pull-sample")
        if sample is None:
            return 0
        buf = sample.get_buffer()
        if buf is None:
            return 0
        success, mapinfo = buf.map(gst.MapFlags.READ)
        if not success:
            return 0
        try:
            data = bytes(mapinfo.data)
            expected = self._width * self._height * 3
            if len(data) < expected:
                self._frames_dropped += 1
                return 0
            try:
                img = Image.frombytes(
                    "RGB", (self._width, self._height), data[:expected]
                )
            except (ValueError, Exception) as exc:  # noqa: BLE001
                self._logger.debug("local_tap_frame_decode_failed", error=str(exc))
                self._frames_dropped += 1
                return 0
            self._frame_holder.set(img)
            now = time.monotonic()
            self._fps_tick_count += 1
            if self._fps_tick_at is None:
                self._fps_tick_at = now
            self._frames_decoded += 1
            self._last_frame_at = now
            if self._first_frame_at is None:
                self._first_frame_at = now
                self._logger.info(
                    "local_tap_first_frame",
                    decoder=self._decoder_type,
                )
            # Any successful frame resets the consecutive-failure counter
            # so the next bus error logs `attempt=1` rather than carrying
            # the prior session's count. Log line stays meaningful.
            self._consecutive_restart_failures = 0
        finally:
            buf.unmap(mapinfo)
        return 0  # Gst.FlowReturn.OK

    def _on_bus_message(self, _bus: Any, message: Any) -> bool:
        """Handle ERROR / EOS / WARNING bus posts and trigger restart."""
        gst = self._Gst
        if gst is None:
            return True
        msg_type = message.type
        if msg_type == gst.MessageType.EOS:
            self._logger.info("local_tap_eos")
            self._pipeline_state = "eos"
            self._maybe_auto_restart(reason="eos")
        elif msg_type == gst.MessageType.ERROR:
            err, dbg = message.parse_error()
            self._logger.warning(
                "local_tap_bus_error",
                error=str(err),
                debug=str(dbg),
            )
            self._pipeline_state = "error"
            self._maybe_auto_restart(reason="error")
        elif msg_type == gst.MessageType.WARNING:
            warn, dbg = message.parse_warning()
            self._logger.debug(
                "local_tap_bus_warning",
                warning=str(warn),
                debug=str(dbg),
            )
        return True

    def _maybe_auto_restart(self, *, reason: str) -> None:
        """Schedule the next restart attempt — fixed 2 s, forever."""
        gst = self._Gst
        pipeline = self._pipeline
        if gst is None or pipeline is None:
            return
        self._consecutive_restart_failures += 1
        self._logger.info(
            "local_tap_restart_scheduled",
            reason=reason,
            delay_s=_RESTART_RETRY_INTERVAL_SECONDS,
            attempt=self._consecutive_restart_failures,
        )
        threading.Timer(
            _RESTART_RETRY_INTERVAL_SECONDS,
            self._do_restart,
            kwargs={"reason": reason},
        ).start()

    def _frame_silence_watchdog(self) -> None:
        """Force a restart when PLAYING but no frame has arrived.

        Catches the failure mode where the gst pipeline reaches PLAYING,
        rtspsrc reconnects, h264parse pushes data, but nothing ever hits
        the appsink (caps mismatch, decoder wedge, queue cycle, etc.).
        The bus-error path doesn't see this because no element emits an
        ERROR — the loop simply runs idle. Without this watchdog the
        FrameSlot keeps the last successfully composited frame and the
        LCD looks frozen until the operator restarts the agent.
        """
        while not self._stop_requested.is_set():
            try:
                time.sleep(_FRAME_SILENCE_POLL_S)
            except Exception:  # noqa: BLE001
                return
            if self._stop_requested.is_set():
                return
            if self._pipeline_state != "playing":
                continue
            last = self._last_frame_at
            if last is None:
                # Pipeline reached PLAYING but no frame ever arrived.
                # The reconnect ladder will eventually catch this; the
                # watchdog only kicks in once we've SEEN at least one
                # frame and then go silent.
                continue
            silent_s = time.monotonic() - last
            if silent_s < _FRAME_SILENCE_THRESHOLD_S:
                continue
            self._logger.warning(
                "local_tap_frame_silence_kick",
                silent_s=round(silent_s, 1),
                threshold_s=_FRAME_SILENCE_THRESHOLD_S,
                latency_samples=self._latency_samples,
                note="appsink starved while PLAYING; forcing restart",
            )
            try:
                self._maybe_auto_restart(reason="frame_silence")
            except Exception as exc:  # noqa: BLE001
                self._logger.warning(
                    "local_tap_watchdog_restart_failed", error=str(exc)
                )
            # After a kick, reset the timer so we don't immediately
            # re-trigger before the new pipeline state takes effect.
            self._last_frame_at = time.monotonic()

    def _do_restart(self, *, reason: str) -> None:
        """Drop to NULL then PLAYING in place to recover from a fault."""
        gst = self._Gst
        pipeline = self._pipeline
        if gst is None or pipeline is None or self._stop_requested.is_set():
            return
        # Zero counters across the restart so stats reflect the new run
        # rather than accumulating across the entire restart-loop
        # history. Without this, latency_samples / sei_miss_count
        # / fps_ewma all carry old values into the new pipeline run
        # and make observability misleading.
        self._reset_stats()
        try:
            pipeline.set_state(gst.State.NULL)
            # Block until the NULL transition actually propagates to
            # every element. Without this, udpsrc may not have released
            # its bound socket by the time we re-enter PLAYING, and the
            # next bind fails with "Address already in use" even with
            # SO_REUSEADDR (the kernel race is brief but real). 1 s is
            # enough on Pi 4B; the timeout prevents an infinite hang
            # if a downstream element refuses to release.
            try:
                pipeline.get_state(1_000_000_000)  # 1 s in ns
            except Exception:  # noqa: BLE001
                pass
            pipeline.set_state(gst.State.PLAYING)
            self._pipeline_state = "playing"
            self._logger.info("local_tap_auto_restart", reason=reason)
        except Exception as exc:  # noqa: BLE001
            self._logger.warning("local_tap_restart_failed", error=str(exc))
            self._pipeline_state = "error"
