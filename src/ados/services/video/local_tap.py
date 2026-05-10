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
import os
import shutil
import subprocess
import threading
import time
from collections import deque
from typing import Any

from PIL import Image

from ados.core.logging import get_logger

log = get_logger("video.local_tap")

# 16-byte UUID prefix the air-side encoder will embed in a SEI of type
# 5 (user_data_unregistered) followed by an 8-byte big-endian uint64 of
# the encoder's wall-clock ``time.time_ns()``. Wall-clock — not
# monotonic — because the air-side and ground-side run on different
# hosts whose monotonic epochs are unrelated. Both ends rely on NTP /
# chrony / systemd-timesyncd to keep wall clocks within a few ms of
# each other, which is the standard assumption on a LAN-paired rig.
ADOS_LATENCY_SEI_UUID = bytes.fromhex("ad05140e9c2c4f6e8a31f0e5b7d4c8a2")
assert len(ADOS_LATENCY_SEI_UUID) == 16

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

# Default MediaMTX path the encoder publishes to. Matches the URL the
# cloud-relay pusher and the websocket relay both consume so the LCD
# tap never sees a frame that hasn't already been validated by the
# rest of the pipeline.
DEFAULT_RTSP_URL = "rtsp://127.0.0.1:8554/main"

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

    Pi 4B (BCM2711) is forced to ``avdec_h264`` because the upstream
    Debian 13 trixie kernel ships a v4l2h264dec / bcm2835-codec combo
    that returns NOT_NEGOTIATED on RTP-depayloaded H.264 streams within
    ~200ms of state-change to PLAYING, regardless of stream-format
    (avc vs byte-stream) or alignment caps. Software decode at 720p
    baseline 30fps consumes ~30% of one A72 core on Pi 4B — well
    inside the headroom — and is correct under the same stream the
    hardware path refuses.
    """
    soc_lower = soc.lower() if isinstance(soc, str) else ""
    if soc_lower.startswith("rk35") and gst_plugin_available("mppvideodec"):
        return "mppvideodec"
    if soc_lower.startswith("rk35") and gst_plugin_available("rkvdec"):
        return "rkvdec"
    if soc_lower.startswith("bcm271"):
        return "avdec_h264"
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


def _iter_nal_units(stream: bytes) -> Any:
    """Yield ``(nal_type, payload)`` tuples from an Annex-B H.264 bytestream.

    Annex-B framing is the on-the-wire byte layout: each NAL unit is
    preceded by either ``00 00 00 01`` or ``00 00 01``. ``h264parse``
    can output either AVC (length-prefixed) or Annex-B; the agent's
    pipeline does not enforce a stream-format, so the parser handles
    both. AVC is detected by the absence of any start code: if no
    start-code prefix is found, we treat the input as length-prefixed
    NAL units with a 4-byte big-endian length header.

    The function is intentionally lenient — a malformed buffer yields
    nothing rather than raising, because the SEI parser is on the hot
    path and a cooked H.264 stream from a real encoder should not
    produce parser exceptions.
    """
    n = len(stream)
    if n < 4:
        return

    # Detect Annex-B: scan for the first 00 00 01 / 00 00 00 01.
    annexb_idx = -1
    i = 0
    while i + 2 < n:
        if stream[i] == 0 and stream[i + 1] == 0:
            if stream[i + 2] == 1:
                annexb_idx = i
                break
            if (
                i + 3 < n
                and stream[i + 2] == 0
                and stream[i + 3] == 1
            ):
                annexb_idx = i
                break
        i += 1

    if annexb_idx >= 0:
        # Annex-B: split on start codes.
        positions: list[int] = []
        i = annexb_idx
        while i + 2 < n:
            if stream[i] == 0 and stream[i + 1] == 0:
                if stream[i + 2] == 1:
                    positions.append(i + 3)
                    i += 3
                    continue
                if (
                    i + 3 < n
                    and stream[i + 2] == 0
                    and stream[i + 3] == 1
                ):
                    positions.append(i + 4)
                    i += 4
                    continue
            i += 1
        for idx, start in enumerate(positions):
            end = (
                positions[idx + 1] - 4
                if idx + 1 < len(positions)
                else n
            )
            # Trim a trailing 00 00 in case the next start code is
            # 00 00 01 (3-byte form).
            while end > start and stream[end - 1] == 0:
                end -= 1
            if end <= start:
                continue
            header = stream[start]
            nal_type = header & 0x1F
            payload = stream[start + 1 : end]
            yield nal_type, payload
        return

    # Length-prefixed (AVC). 4-byte big-endian length header per NAL.
    i = 0
    while i + 4 <= n:
        length = int.from_bytes(stream[i : i + 4], "big")
        i += 4
        if length <= 0 or i + length > n:
            return
        if length < 1:
            continue
        header = stream[i]
        nal_type = header & 0x1F
        payload = stream[i + 1 : i + length]
        yield nal_type, payload
        i += length


def parse_sei_latency_ns(stream: bytes) -> int | None:
    """Extract the air-side encoder's wall-clock-time-ns from a SEI marker.

    Scans ``stream`` for an H.264 SEI NAL unit (NAL type 6) that
    contains a user-data-unregistered payload (payload type 5) whose
    16-byte UUID matches :data:`ADOS_LATENCY_SEI_UUID`. The next 8
    bytes are interpreted as a big-endian uint64 of the encoder's
    ``time.time_ns()`` at frame-encode time. Wall-clock so a comparison
    against the receiver's ``time.time_ns()`` produces meaningful
    glass-to-glass latency across hosts.

    Returns the encoded ns value, or ``None`` if no matching SEI is
    present in the buffer.
    """
    for nal_type, payload in _iter_nal_units(stream):
        if nal_type != 6:
            continue
        if not payload:
            continue
        # SEI message structure: <payload_type> <payload_size> <data>.
        # payload_type and payload_size are each ff-extended bytes per
        # the spec but in practice fit in one byte for our markers.
        idx = 0
        plen = len(payload)
        while idx < plen:
            ptype = 0
            while idx < plen and payload[idx] == 0xFF:
                ptype += 0xFF
                idx += 1
            if idx >= plen:
                break
            ptype += payload[idx]
            idx += 1
            psize = 0
            while idx < plen and payload[idx] == 0xFF:
                psize += 0xFF
                idx += 1
            if idx >= plen:
                break
            psize += payload[idx]
            idx += 1
            if idx + psize > plen:
                break
            data = payload[idx : idx + psize]
            idx += psize
            if (
                ptype == 5
                and len(data) >= 16 + 8
                and data[:16] == ADOS_LATENCY_SEI_UUID
            ):
                ns = int.from_bytes(data[16 : 16 + 8], "big", signed=False)
                return ns
        # Continue to the next NAL unit in case there are multiple.
    return None


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
        # queue between depay and decoder hands rtspsrc its own thread so
        # the network reader keeps pulling RTP while the decoder works.
        # leaky=downstream + small max-size means a slow decoder drops
        # the oldest frame instead of blocking upstream and starving
        # rtspsrc's RTCP loop (which cascades into "not-linked" errors).
        "! queue max-size-buffers=8 max-size-bytes=0 max-size-time=0 leaky=downstream "
        f"! {decoder} "
        # second queue between decoder and downstream conversion gives
        # the decoder its own thread. PIL composition + framebuffer write
        # in the render loop runs at 20 Hz and shares the GIL with the
        # appsink callback; without this queue, a slow render tick stalls
        # the decoder.
        "! queue max-size-buffers=4 max-size-bytes=0 max-size-time=0 leaky=downstream "
        # decimate the decoded stream to 15 fps before videoconvert.
        # The LCD render loop runs at ~20 Hz on a single Python GIL,
        # competing with PIL composition + SPI framebuffer write that
        # can take 30-50 ms per render tick. At 30 fps source, each
        # appsink callback contends with the render loop and frames
        # back-pressure into the appsink queue; the visible result is
        # smooth video for a few seconds then a freeze when the buffer
        # saturates. At 15 fps the callback rate matches what the
        # render loop can consume between SPI writes. drop-only=true
        # reuses the most-recent frame instead of duplicating, so we
        # never invent frames on slow inputs.
        "! videorate drop-only=true "
        "! video/x-raw,framerate=15/1 "
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

            # Best-effort pad probe on the h264parse src pad for SEI
            # latency markers. The element is unnamed in the pipeline
            # string; iterate the pipeline to find it. If it cannot be
            # located the rest of the tap still works — latency just
            # stays unset.
            h264parse = self._find_h264parse(pipeline)
            if h264parse is not None:
                src_pad = h264parse.get_static_pad("src")
                if src_pad is not None:
                    try:
                        probe_mask = Gst.PadProbeType.BUFFER
                        probe_id = src_pad.add_probe(
                            probe_mask, self._on_h264_buffer, None,
                        )
                        self._h264parse = h264parse
                        self._h264parse_probe_id = probe_id
                    except Exception as exc:  # noqa: BLE001
                        self._logger.debug(
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
            return 1
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
    "ADOS_LATENCY_SEI_UUID",
    "DEFAULT_HEIGHT",
    "DEFAULT_RTSP_URL",
    "DEFAULT_WIDTH",
    "LocalVideoTap",
    "LocalVideoTapUnavailable",
    "build_pipeline_string",
    "gst_plugin_available",
    "parse_sei_latency_ns",
    "select_decoder",
]
