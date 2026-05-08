"""Local video tap that decodes the MediaMTX RTSP feed for the LCD page.

The ADOS Mission Control's video page on the SPI LCD wants a live H.264
preview at the panel's native resolution. Rather than re-decoding through
the MediaMTX HTTP/WHEP layer, we shell into the same gstreamer userspace
the encoder pipeline already depends on, attach an ``appsink`` to the
``rtsp://127.0.0.1:8554/main`` path, and surface the latest decoded
frame as a PIL ``Image`` for the page renderer to composite into its
canvas.

Decoder selection prefers the platform's hardware path when available
(``mppvideodec`` on Rockchip, ``v4l2h264dec`` on Allwinner), then falls
back to ``avdec_h264`` for x86 dev hosts and SBCs without a working
hardware decoder. Selection is cached per process; ``gst-inspect-1.0``
gets shelled exactly once per plugin name.

PyGObject is loaded via the system ``python3-gi`` apt package on
Debian. We do NOT add it to pyproject dependencies because pip-built
PyGObject compiles against ``gobject-2.0`` and ``libgirepository`` which
are notoriously fragile to version drift on Bookworm rootfs; the apt
package is the supported path. When the import fails on a rig that
hasn't run ``install.sh`` yet, ``LocalVideoTap.start()`` raises
:class:`LocalVideoTapUnavailable` so the page can render a fail-soft
"Video pipeline unavailable" card instead of crashing the LCD service.

Lifecycle
---------

* ``start()`` constructs the pipeline lazily, transitions PAUSED ->
  PLAYING on a daemon thread that owns its own ``GMainLoop``. The first
  ``new-sample`` callback timestamps ``_first_frame_at``.
* ``pause()`` / ``resume()`` are fast — just a state-change call on
  the existing pipeline. Used when the operator switches tabs and
  comes back inside the inactivity grace.
* ``stop()`` posts EOS, waits up to 1 s, then sets state NULL and
  joins the loop thread.

Restart policy
--------------

The bus error handler logs the failure and tries one auto-restart with
2 s backoff. After three failures within a 30 s window we give up and
flip ``pipeline_state`` to ``"failed"`` so the page renders the fail-
soft card. The threshold mirrors what the MediaMTX manager uses for
encoder restarts — long enough to ride out a USB reset, short enough
to surface a stuck pipeline before the operator wonders why preview is
black.
"""

from __future__ import annotations

import asyncio
import shutil
import subprocess
import threading
import time
from collections import deque
from typing import Any

from PIL import Image

from ados.core.logging import get_logger

log = get_logger("video.local_tap")

# Default MediaMTX path the encoder publishes to. Matches the URL the
# cloud-relay pusher and the websocket relay both consume so the LCD
# tap never sees a frame that hasn't already been validated by the
# rest of the pipeline.
DEFAULT_RTSP_URL = "rtsp://127.0.0.1:8554/main"

# Target geometry for the LCD video region (480 px wide, 176 px tall —
# leaves a 56 px metrics strip below).
DEFAULT_WIDTH = 480
DEFAULT_HEIGHT = 176

# Bus auto-restart policy.
_MAX_RESTART_ATTEMPTS = 3
_RESTART_BACKOFF_SECONDS = 2.0
_RESTART_FAILURE_WINDOW_SECONDS = 30.0

# Reconnect ladder for the RTSP source itself when the pipeline starts
# but never receives a frame. Capped at 30 s so a long encoder outage
# doesn't compound.
_RECONNECT_LADDER_SECONDS: tuple[float, ...] = (1.0, 2.0, 4.0, 8.0, 30.0)


class LocalVideoTapUnavailable(RuntimeError):  # noqa: N818
    """Raised by :meth:`LocalVideoTap.start` when gstreamer is missing.

    Carries a short reason the page can surface to the operator. The LCD
    service catches this exception and renders a fail-soft card instead
    of stopping the service.
    """


class _FrameSlot:
    """Single-slot atomic frame holder.

    The appsink callback runs on the gstreamer streaming thread; the
    page renderer runs in the asyncio loop. We don't need a queue —
    only the latest frame matters. A bare ``threading.Lock`` around a
    single attribute is enough; the lock is held only while swapping
    the reference, never during decode.
    """

    def __init__(self) -> None:
        self._frame: Image.Image | None = None
        self._lock = threading.Lock()

    def set(self, frame: Image.Image | None) -> None:
        with self._lock:
            self._frame = frame

    def get(self) -> Image.Image | None:
        with self._lock:
            return self._frame


class _PluginInspector:
    """Caches ``gst-inspect-1.0`` results so we don't re-shell per call."""

    def __init__(self) -> None:
        self._cache: dict[str, bool] = {}
        self._lock = threading.Lock()

    def available(self, plugin: str) -> bool:
        with self._lock:
            cached = self._cache.get(plugin)
            if cached is not None:
                return cached
        present = self._shell_check(plugin)
        with self._lock:
            self._cache[plugin] = present
        return present

    @staticmethod
    def _shell_check(plugin: str) -> bool:
        if shutil.which("gst-inspect-1.0") is None:
            return False
        try:
            result = subprocess.run(  # noqa: S603 — fixed binary, fixed args
                ["gst-inspect-1.0", plugin],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                timeout=2.0,
                check=False,
            )
        except (subprocess.TimeoutExpired, OSError):
            return False
        return result.returncode == 0


_INSPECTOR = _PluginInspector()


def gst_plugin_available(name: str) -> bool:
    """Return True when ``gst-inspect-1.0 <name>`` exits 0.

    Result is cached per-process; the first call shells out, subsequent
    calls hit the in-memory map. A missing ``gst-inspect-1.0`` binary
    short-circuits to False so a rig without gstreamer userspace tooling
    never blocks the render loop.
    """
    return _INSPECTOR.available(name)


def select_decoder(soc: str) -> str:
    """Pick the best gstreamer H.264 decoder for the running SoC.

    The order mirrors the encoder selection in the existing video
    pipeline: prefer Rockchip's MPP path, then the upstream V4L2 path
    (works on a wide swath of Allwinner / Amlogic), then software.

    The returned name is the gstreamer element factory string the
    pipeline string should embed.
    """
    soc_lower = soc.lower() if isinstance(soc, str) else ""
    if soc_lower.startswith("rk35") and gst_plugin_available("mppvideodec"):
        return "mppvideodec"
    if soc_lower.startswith("rk35") and gst_plugin_available("rkvdec"):
        return "rkvdec"
    if gst_plugin_available("v4l2h264dec"):
        return "v4l2h264dec"
    return "avdec_h264"


def _detect_soc() -> str:
    """Return the SoC string from the HAL, or an empty string on miss.

    Imported lazily so this module doesn't fail to import on a host
    whose HAL stack is incomplete (CI runners, dev laptops).
    """
    try:
        from ados.hal.detect import detect_board

        board = detect_board()
        return getattr(board, "soc", "") or ""
    except Exception:  # noqa: BLE001
        return ""


def build_pipeline_string(
    *,
    source_url: str,
    decoder: str,
    width: int,
    height: int,
    latency_ms: int,
) -> str:
    """Compose the gstreamer pipeline launch string.

    Kept as a pure function so tests can assert the rendered string per
    SoC without spinning up gstreamer. Format mirrors the OEM doc set
    so a bench operator pasting the same line into ``gst-launch-1.0``
    sees the same behavior.
    """
    return (
        f"rtspsrc location={source_url} protocols=tcp "
        f"latency={latency_ms} drop-on-latency=true "
        "! rtph264depay "
        "! h264parse "
        f"! {decoder} "
        "! videoconvert "
        "! videoscale "
        f"! video/x-raw,format=RGB,width={width},height={height} "
        "! appsink name=tap max-buffers=2 drop=true emit-signals=true sync=false"
    )


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
        logger: Any | None = None,
    ) -> None:
        self._source_url = source_url
        self._width = width
        self._height = height
        self._logger = logger or log

        self._frame_holder = _FrameSlot()
        self._frames_decoded: int = 0
        self._frames_dropped: int = 0
        self._first_frame_at: float | None = None
        self._last_frame_at: float | None = None
        self._fps_window: deque[float] = deque(maxlen=30)
        self._decoder_type: str | None = None
        self._pipeline_state: str = "idle"
        self._restart_failures: deque[float] = deque(maxlen=_MAX_RESTART_ATTEMPTS)

        # Lazily-bound gstreamer / PyGObject objects. Held as Any so the
        # type checker doesn't need PyGObject installed in CI.
        self._Gst: Any | None = None
        self._GLib: Any | None = None
        self._pipeline: Any | None = None
        self._appsink: Any | None = None
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
            latency_ms = 50 if decoder != "avdec_h264" else 100
            pipeline_str = build_pipeline_string(
                source_url=self._source_url,
                decoder=decoder,
                width=self._width,
                height=self._height,
                latency_ms=latency_ms,
            )
            self._logger.info(
                "local_tap_pipeline_constructed",
                decoder=decoder,
                source_url=self._source_url,
                width=self._width,
                height=self._height,
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
            return
        self._stop_requested.set()
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
            self._loop = None
            self._thread = None
            self._pipeline_state = "stopped"
        self._logger.info("local_tap_stopped")

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
        }

    # ── internals ──────────────────────────────────────────────

    def _compute_fps(self) -> float:
        """Average FPS across the rolling 30-sample window."""
        window = list(self._fps_window)
        if len(window) < 2:
            return 0.0
        span = window[-1] - window[0]
        if span <= 0:
            return 0.0
        return (len(window) - 1) / span

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
            self._fps_window.append(now)
            self._frames_decoded += 1
            self._last_frame_at = now
            if self._first_frame_at is None:
                self._first_frame_at = now
                self._logger.info(
                    "local_tap_first_frame",
                    decoder=self._decoder_type,
                )
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
        """Try a single restart with backoff; surface failure cap to UI."""
        now = time.monotonic()
        # Trim failures outside the rolling window so a single 30-min
        # outage doesn't permanently mark the tap as failed.
        while (
            self._restart_failures
            and (now - self._restart_failures[0]) > _RESTART_FAILURE_WINDOW_SECONDS
        ):
            self._restart_failures.popleft()
        if len(self._restart_failures) >= _MAX_RESTART_ATTEMPTS:
            self._pipeline_state = "failed"
            self._logger.warning(
                "local_tap_restart_cap_hit",
                reason=reason,
                window_s=_RESTART_FAILURE_WINDOW_SECONDS,
            )
            return
        self._restart_failures.append(now)
        gst = self._Gst
        pipeline = self._pipeline
        if gst is None or pipeline is None:
            return
        threading.Timer(
            _RESTART_BACKOFF_SECONDS,
            self._do_restart,
            kwargs={"reason": reason},
        ).start()

    def _do_restart(self, *, reason: str) -> None:
        """Drop to NULL then PLAYING in place to recover from a fault."""
        gst = self._Gst
        pipeline = self._pipeline
        if gst is None or pipeline is None or self._stop_requested.is_set():
            return
        try:
            pipeline.set_state(gst.State.NULL)
            pipeline.set_state(gst.State.PLAYING)
            self._pipeline_state = "playing"
            self._logger.info("local_tap_auto_restart", reason=reason)
        except Exception as exc:  # noqa: BLE001
            self._logger.warning("local_tap_restart_failed", error=str(exc))
            self._pipeline_state = "error"


__all__ = [
    "DEFAULT_HEIGHT",
    "DEFAULT_RTSP_URL",
    "DEFAULT_WIDTH",
    "LocalVideoTap",
    "LocalVideoTapUnavailable",
    "build_pipeline_string",
    "gst_plugin_available",
    "select_decoder",
]
