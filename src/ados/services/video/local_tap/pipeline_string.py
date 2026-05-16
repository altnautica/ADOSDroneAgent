"""GStreamer pipeline-string builder + decoder selection helpers.

Pure helpers split out of the tap module so test runners can assert
the rendered pipeline string per SoC / source URL without spinning up
GStreamer. The ``LocalVideoTap`` class composes these to construct
``Gst.parse_launch`` input.
"""

from __future__ import annotations

import shutil
import subprocess
import threading


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
    # Resolve ``gst_plugin_available`` against the barrel module so
    # tests can monkeypatch the package-level binding and have it take
    # effect here. Late ``import`` keeps the load order safe; the
    # barrel imports this module then attribute-binds the symbol back,
    # so by the time ``select_decoder`` runs both bindings exist.
    from ados.services.video import local_tap as _pkg

    available = _pkg.gst_plugin_available
    soc_lower = soc.lower() if isinstance(soc, str) else ""
    if soc_lower.startswith("rk35") and available("mppvideodec"):
        return "mppvideodec"
    if soc_lower.startswith("rk35") and available("rkvdec"):
        return "rkvdec"
    if soc_lower.startswith("bcm271"):
        return "avdec_h264"
    if available("v4l2h264dec"):
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
    fps_cap: int = 15,
) -> str:
    """Compose the gstreamer pipeline launch string.

    Kept as a pure function so tests can assert the rendered string per
    SoC without spinning up gstreamer. Format mirrors the OEM doc set
    so a bench operator pasting the same line into ``gst-launch-1.0``
    sees the same behavior.

    ``fps_cap`` is the videorate decimation target. Default 15 matches
    the SPI-write throughput of the Pi 4B + Waveshare 3.5" combo;
    higher values are achievable on faster SBCs / hardware decoders.

    ``source_url`` selects the front end:
    - If it starts with ``rtsp://`` (legacy / tests), we use rtspsrc.
    - Otherwise we treat it as an integer UDP port and use udpsrc with
      RTP H.264 caps. This is the Phase 11 default — it bypasses the
      mediamtx-gs RTSP indirection and the rtspsrc 100 ms jitter buffer
      that was the source of the freeze cascade.
    """
    if source_url.startswith("rtsp://"):
        # Legacy rtspsrc front end — kept so tests that pre-date the
        # udpsrc path continue to work, and so a bench operator can
        # opt into the rtspsrc+mediamtx-gs path for debugging.
        front = (
            f"rtspsrc location={source_url} protocols=tcp "
            f"latency={latency_ms} drop-on-latency=true "
            "! rtph264depay "
        )
    else:
        # Phase 11 default: udpsrc directly from the LCD-side fanout
        # port. No mediamtx-gs round-trip, no rtspsrc TCP handshake,
        # no 404 race when mediamtx-gs goes briefly not-ready.
        #
        # Bench-tested rtpjitterbuffer settings:
        # - drop the `mode=0 do-lost=true` knobs we initially tried;
        #   `do-lost=true` was triggering downstream EOS events that
        #   tore down the pipeline within seconds of pipeline start.
        # - default mode (slave) + plain latency=50 keeps the buffer
        #   simple and lets gst handle packet loss via leaky queues
        #   downstream like the rest of the chain.
        try:
            udp_port = int(source_url)
        except (ValueError, TypeError):
            udp_port = 5605
        # `reuse=true` enables SO_REUSEADDR + SO_REUSEPORT so a quick
        # restart cycle (start() retry after a transient pipeline
        # construction failure) doesn't get "Address already in use"
        # while the previous udpsrc's NULL-state transition propagates.
        front = (
            f"udpsrc port={udp_port} reuse=true "
            "caps=\"application/x-rtp,media=video,encoding-name=H264,"
            "payload=96,clock-rate=90000\" "
            f"! rtpjitterbuffer latency={latency_ms} "
            "! rtph264depay "
        )
    return (
        f"{front}"
        # h264parse with default settings, named so the SEI pad probe
        # can be pinned via get_by_name. We tried adding alignment=au
        # (both as a downstream capsfilter and as an element property)
        # AND config-interval=1 (SPS/PPS injection); both broke the
        # pipeline on Pi 4B's GStreamer with `streaming stopped,
        # reason not-linked` errors that put the tap into a sustained
        # restart loop. The SEI pad probe walks the buffer's byte
        # stream looking for our UUID, so it works without any explicit
        # alignment. SPS/PPS for late-joiners is taken care of by the
        # encoder side (libx264 -bsf:v h264_mp4toannexb already
        # inlines SPS/PPS at IDRs).
        "! h264parse name=h264parse_tap "
        # queue between depay and decoder hands the source its own
        # thread so the network reader keeps pulling RTP while the
        # decoder works. leaky=downstream + max-size-buffers=8 means
        # a slow decoder drops the oldest frame instead of blocking
        # upstream.
        "! queue max-size-buffers=8 max-size-bytes=0 max-size-time=0 leaky=downstream "
        f"! {decoder} "
        # second queue between decoder and downstream conversion gives
        # the decoder its own thread. PIL composition + framebuffer write
        # in the render loop runs at 20 Hz and shares the GIL with the
        # appsink callback; without this queue, a slow render tick stalls
        # the decoder.
        "! queue max-size-buffers=4 max-size-bytes=0 max-size-time=0 leaky=downstream "
        # decimate the decoded stream to fps_cap before videoconvert.
        # Default 15 matches what the Pi 4B's single-threaded Python GIL
        # + PIL composition + SPI framebuffer write loop can sustain
        # without back-pressuring the appsink queue and freezing video.
        # Faster SBCs / hardware decoders / lighter LCD render paths
        # can raise this via WfbConfig.lcd_fps_cap. drop-only=true reuses
        # the most-recent frame instead of duplicating.
        "! videorate drop-only=true "
        f"! video/x-raw,framerate={int(fps_cap)}/1 "
        "! videoconvert "
        "! videoscale "
        f"! video/x-raw,format=RGB,width={width},height={height} "
        "! appsink name=tap max-buffers=2 drop=true emit-signals=true sync=false"
    )
