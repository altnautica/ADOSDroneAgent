"""Video page — live H.264 preview from the local MediaMTX feed.

The page reserves the top 480x176 region of the LCD content area for
the decoded video frame and the bottom 480x68 strip for a permanent
metrics row. The :class:`LocalVideoTap` is constructed once on first
``on_enter``, kept paused when the operator is on a different tab,
and torn down after a 30 s inactivity grace.

A REC chip in the top-left of the video region toggles the agent's
recording state via ``POST /api/video/record/{start,stop}``. A camera
switch chip in the top-right opens an :class:`EnumPickerModal` listing
the cameras the agent has enumerated; the picker shape is wired now
so that when ``GET /api/video/cameras`` lands in a follow-on commit the
swap-out only requires a payload change.

Tapping anywhere else inside the video region toggles the optional
"detail HUD" overlay that surfaces decoder type, FPS, FEC drops, and
the rolling bitrate. The overlay is a translucent panel painted on top
of the video plane so the operator doesn't lose the live frame while
inspecting telemetry.
"""

from __future__ import annotations

import asyncio
import time
from collections import deque
from typing import Any, ClassVar

from PIL import Image, ImageDraw

from ados.services.ui.dashboards.components import primitives as p
from ados.services.ui.touch.events import TouchGesture
from ados.services.ui.widgets.camera_chip import draw_camera_chip
from ados.services.ui.widgets.enum_picker import EnumPickerModal
from ados.services.ui.widgets.rec_button import draw_rec_button
from ados.services.ui.widgets.video_compositor import VideoCompositor
from ados.services.video.local_tap import (
    LocalVideoTap,
    LocalVideoTapUnavailable,
)

from .base import HitZone, PageContext

PAGE_W = 480
PAGE_H = 244
VIDEO_H = 176
METRICS_H = PAGE_H - VIDEO_H  # 68 px

# Refresh budget for the metrics overlay. The video frame itself paints
# at the page's refresh_hz; metrics tick at 1 Hz on a background task.
_METRICS_REFRESH_SECONDS = 1.0

# Inactivity grace before we tear down the local tap. Matches the spec
# in the OLED service's _render_forever path.
_TAP_INACTIVITY_TEARDOWN_SECONDS = 30.0


def _safe_dict(value: Any) -> dict:
    return value if isinstance(value, dict) else {}


class VideoPage:
    """Top-level Video tab content."""

    id: ClassVar[str] = "video"
    refresh_hz: ClassVar[float] = 20.0

    def __init__(self) -> None:
        self._compositor = VideoCompositor()
        self._tap: LocalVideoTap | None = None
        self._tap_unavailable_reason: str | None = None
        self._recording: bool = False
        self._camera_count: int = 1
        self._camera_label: str = "CAM 1"
        self._cameras: list[dict[str, Any]] = []
        self._show_detail_hud: bool = False
        self._metrics_cache: dict[str, Any] = {
            "bitrate_kbps": None,
            "rssi_dbm": None,
            "fec_drops": None,
            "channel": None,
            "mcs_index": None,
            "latency_ms": None,
        }
        self._metrics_task: asyncio.Task | None = None
        self._mediamtx_prev_bytes: int | None = None
        self._mediamtx_prev_at: float | None = None
        # Bitrate sparkline ring for the detail HUD; 60 samples = 60 s.
        self._bitrate_history: deque[float] = deque(maxlen=60)
        self._last_active_at: float = time.monotonic()

    # ── lifecycle ──────────────────────────────────────────────

    async def on_enter(self, ctx: PageContext) -> None:
        ctx.logger.info("video_enter")
        self._last_active_at = time.monotonic()
        await self._ensure_tap(ctx)
        if self._metrics_task is None or self._metrics_task.done():
            self._metrics_task = asyncio.create_task(
                self._refresh_metrics_forever(ctx),
                name="video_metrics",
            )

    async def on_leave(self, ctx: PageContext) -> None:
        ctx.logger.info("video_leave")
        # Pause but don't stop the tap — the operator may come right
        # back. The render loop tears it down on inactivity.
        if self._tap is not None:
            try:
                await self._tap.pause()
            except Exception as exc:  # noqa: BLE001
                ctx.logger.debug("video_tap_pause_failed", error=str(exc))
        if self._metrics_task is not None and not self._metrics_task.done():
            self._metrics_task.cancel()
            try:
                await self._metrics_task
            except (asyncio.CancelledError, Exception):
                pass
        self._metrics_task = None

    async def _ensure_tap(self, ctx: PageContext) -> None:
        if self._tap is not None:
            try:
                await self._tap.resume()
            except Exception as exc:  # noqa: BLE001
                ctx.logger.debug("video_tap_resume_failed", error=str(exc))
            return
        if self._tap_unavailable_reason is not None:
            return
        tap = LocalVideoTap(logger=ctx.logger)
        try:
            await tap.start()
        except LocalVideoTapUnavailable as exc:
            self._tap_unavailable_reason = str(exc)
            ctx.logger.warning("video_tap_unavailable", reason=str(exc))
            return
        except Exception as exc:  # noqa: BLE001
            self._tap_unavailable_reason = str(exc)
            ctx.logger.warning("video_tap_start_failed", error=str(exc))
            return
        self._tap = tap

    async def maybe_teardown_idle_tap(self, ctx: PageContext) -> None:
        """Stop the tap if the operator hasn't been on this tab for a while.

        Called by the OLED service render loop while the active page is
        NOT the video page — the page can't observe its own absence
        directly, so the loop drives the timer based on the last
        ``on_enter`` timestamp.
        """
        if self._tap is None:
            return
        idle = time.monotonic() - self._last_active_at
        if idle < _TAP_INACTIVITY_TEARDOWN_SECONDS:
            return
        try:
            await self._tap.stop()
        except Exception as exc:  # noqa: BLE001
            ctx.logger.debug("video_tap_idle_stop_failed", error=str(exc))
        self._tap = None

    # ── metrics refresher ─────────────────────────────────────

    async def _refresh_metrics_forever(self, ctx: PageContext) -> None:
        try:
            while True:
                await self._refresh_metrics_once(ctx)
                await asyncio.sleep(_METRICS_REFRESH_SECONDS)
        except asyncio.CancelledError:
            return
        except Exception as exc:  # noqa: BLE001
            ctx.logger.debug("video_metrics_loop_failed", error=str(exc))

    async def _refresh_metrics_once(self, ctx: PageContext) -> None:
        client = ctx.http
        if client is None:
            return
        # MediaMTX bytes-received delta → bitrate.
        try:
            r = await client.get(
                "http://127.0.0.1:9997/v3/paths/get/main", timeout=1.5,
            )
            if getattr(r, "status_code", None) == 200:
                blob = r.json() if callable(getattr(r, "json", None)) else {}
                if isinstance(blob, dict):
                    self._update_bitrate(blob)
        except Exception as exc:  # noqa: BLE001
            ctx.logger.debug("video_metrics_mediamtx_failed", error=str(exc))
        # RSSI / FEC drops: prefer the in-process LinkQualityMonitor
        # because the data is already in memory and never racy with the
        # REST surface. Channel / MCS still come from the REST blob —
        # those values originate from config, not the rx parser.
        wfb_blob = await self._load_wfb_blob(ctx, client)
        if wfb_blob is not None:
            self._metrics_cache["channel"] = wfb_blob.get("channel")
            self._metrics_cache["mcs_index"] = wfb_blob.get("mcs_index")
            rssi = wfb_blob.get("rssi_dbm")
            if isinstance(rssi, (int, float)):
                # The REST surface emits -100.0 as a "no signal" sentinel
                # rather than null; let it through so the operator sees
                # something rather than "—" indefinitely on a healthy
                # but quiet link. The metrics formatter renders it as
                # "-100 dBm" which the operator can read at a glance.
                self._metrics_cache["rssi_dbm"] = float(rssi)
            fec_recovered = wfb_blob.get("fec_recovered")
            fec_failed = wfb_blob.get("fec_failed")
            if isinstance(fec_recovered, (int, float)) and isinstance(
                fec_failed, (int, float),
            ):
                lost = int(fec_failed)
                rec = int(fec_recovered)
                total = rec + lost
                self._metrics_cache["fec_drops"] = (lost, total)
            else:
                # Some legacy callers emit a plain int "fec_drops" key.
                drops = wfb_blob.get("fec_drops")
                if isinstance(drops, tuple) and len(drops) == 2:
                    self._metrics_cache["fec_drops"] = drops
                elif isinstance(drops, (int, float)):
                    self._metrics_cache["fec_drops"] = (int(drops), int(drops))
        # Latency comes from the local tap's SEI parser; refresh the
        # cached value so a paused tab still shows the most recent
        # number rather than a flat None.
        if self._tap is not None:
            stats = self._tap.stats()
            latency_ms = stats.get("latency_ms")
            if isinstance(latency_ms, (int, float)):
                self._metrics_cache["latency_ms"] = float(latency_ms)
            else:
                self._metrics_cache["latency_ms"] = None
        # Recording state from the consolidated status endpoint.
        try:
            r = await client.get("/api/status/full", timeout=1.5)
            status_code = getattr(r, "status_code", None)
            if status_code == 404:
                # Fall back to the ground-station status which is always
                # there. The recording flag may be absent — that's fine.
                r = await client.get(
                    "/api/v1/ground-station/status", timeout=1.5,
                )
                status_code = getattr(r, "status_code", None)
            if status_code == 200:
                blob = r.json() if callable(getattr(r, "json", None)) else {}
                video_block = _safe_dict(blob.get("video"))
                if "recording" in video_block:
                    self._recording = bool(video_block.get("recording"))
        except Exception as exc:  # noqa: BLE001
            ctx.logger.debug("video_metrics_status_failed", error=str(exc))
        # Camera enumeration.
        try:
            r = await client.get("/api/video/cameras", timeout=1.5)
            if getattr(r, "status_code", None) == 200:
                blob = r.json() if callable(getattr(r, "json", None)) else {}
                cameras = blob.get("cameras") if isinstance(blob, dict) else None
                if isinstance(cameras, list) and cameras:
                    self._cameras = [c for c in cameras if isinstance(c, dict)]
                    self._camera_count = len(self._cameras)
                    active_idx = 0
                    for i, cam in enumerate(self._cameras):
                        if cam.get("active"):
                            active_idx = i
                            break
                    label = self._cameras[active_idx].get("label") or self._cameras[
                        active_idx
                    ].get("name")
                    self._camera_label = (
                        str(label) if label else f"CAM {active_idx + 1}"
                    )
        except Exception as exc:  # noqa: BLE001
            ctx.logger.debug("video_metrics_cameras_failed", error=str(exc))
        # Publish a tiny tap-status snapshot to /run/ados/lcd-video-tap.json
        # so the cloud heartbeat enricher can surface decoder state to
        # the GCS without reaching into the OLED service's process.
        try:
            self._publish_tap_status()
        except Exception as exc:  # noqa: BLE001
            ctx.logger.debug("video_tap_status_publish_failed", error=str(exc))

    def _publish_tap_status(self) -> None:
        """Atomic-write a tap-status JSON for the heartbeat enricher.

        The payload is intentionally minimal: just the four fields the
        GCS Display sub-view reads (active, decoder, fps, recording).
        Written every metrics tick (~1 Hz) so the heartbeat — which
        also runs at 5 s — never reads more than a 1 s old snapshot.
        """
        from ados.core.paths import LCD_VIDEO_TAP_PATH

        active = False
        decoder: str | None = None
        fps: float = 0.0
        if self._tap is not None:
            stats = self._tap.stats()
            pipeline_state = str(stats.get("pipeline_state") or "")
            active = pipeline_state == "playing"
            decoder = stats.get("decoder_type") or None
            fps_val = stats.get("fps")
            fps = float(fps_val) if isinstance(fps_val, (int, float)) else 0.0
        blob: dict[str, Any] = {
            "active": active,
            "decoder": decoder,
            "fps": round(fps, 2),
            "recording": bool(self._recording),
            "updated_at_ms": int(time.time() * 1000),
        }
        _write_tap_status(LCD_VIDEO_TAP_PATH, blob)

    async def _load_wfb_blob(
        self,
        ctx: PageContext,
        client: Any,
    ) -> dict | None:
        """Return a {channel, mcs_index, rssi_dbm, fec_*} dict.

        Tries the in-process ``LinkQualityMonitor`` first because the
        OLED service runs in the same process as the agent core when
        the ``ados-oled`` unit is colocated with ``ados-agent``. When
        the lookup fails — typical inside the test suite where
        ``get_agent_app`` is uninitialized — falls back to the
        ``/api/wfb`` REST endpoint via the supplied HTTP client.
        """
        # Try in-process first.
        try:
            from ados.api.deps import get_agent_app

            app = get_agent_app()
            wfb = app.wfb_manager()
            if wfb is not None:
                monitor = getattr(wfb, "monitor", None)
                cfg = getattr(app, "config", None)
                wfb_cfg = (
                    getattr(getattr(cfg, "video", None), "wfb", None)
                    if cfg is not None
                    else None
                )
                channel = getattr(wfb, "_channel", None)
                if channel is None and wfb_cfg is not None:
                    channel = getattr(wfb_cfg, "channel", None)
                mcs_index = (
                    getattr(wfb_cfg, "mcs_index", None)
                    if wfb_cfg is not None
                    else None
                )
                if monitor is not None:
                    snap = monitor.get_current()
                    return {
                        "channel": channel,
                        "mcs_index": mcs_index,
                        "rssi_dbm": float(snap.rssi_dbm),
                        "fec_recovered": int(snap.fec_recovered),
                        "fec_failed": int(snap.fec_failed),
                    }
        except (AssertionError, ImportError, AttributeError, Exception) as exc:  # noqa: BLE001
            ctx.logger.debug("video_metrics_wfb_inproc_failed", error=str(exc))
        # Fall back to REST.
        try:
            r = await client.get("/api/wfb", timeout=1.5)
            if getattr(r, "status_code", None) == 200:
                blob = r.json() if callable(getattr(r, "json", None)) else {}
                if isinstance(blob, dict):
                    return blob
        except Exception as exc:  # noqa: BLE001
            ctx.logger.debug("video_metrics_wfb_failed", error=str(exc))
        return None

    def _update_bitrate(self, mediamtx_blob: dict) -> None:
        # MediaMTX has used both ``bytesReceived`` (v1.0+) and
        # ``bytes_received`` (older builds shipped via apt). Accept
        # either, ignoring NaN / negative deltas (which can happen if
        # MediaMTX restarts and the counter resets).
        bytes_received = mediamtx_blob.get("bytesReceived")
        if not isinstance(bytes_received, (int, float)):
            bytes_received = mediamtx_blob.get("bytes_received")
        if not isinstance(bytes_received, (int, float)) or bytes_received < 0:
            return
        now = time.monotonic()
        prev_bytes = self._mediamtx_prev_bytes
        prev_at = self._mediamtx_prev_at
        self._mediamtx_prev_bytes = int(bytes_received)
        self._mediamtx_prev_at = now
        if prev_bytes is None or prev_at is None:
            return
        dt = now - prev_at
        if dt <= 0:
            return
        raw_delta = int(bytes_received) - prev_bytes
        # A counter reset (MediaMTX restart) shows up as a large
        # negative; clip to 0 so the next tick shows the fresh
        # counter delta rather than a giant spike.
        delta = max(0, raw_delta)
        kbps = (delta * 8.0 / 1000.0) / dt
        self._metrics_cache["bitrate_kbps"] = kbps
        self._bitrate_history.append(kbps)

    # ── render ─────────────────────────────────────────────────

    async def render(self, ctx: PageContext) -> Image.Image:
        self._last_active_at = time.monotonic()
        palette = ctx.palette
        canvas = Image.new("RGB", (PAGE_W, PAGE_H), palette.bg_primary)

        frame: Image.Image | None = None
        if self._tap_unavailable_reason is not None:
            self._compositor.paint(
                canvas,
                0,
                0,
                palette=palette,
                frame=None,
                width=PAGE_W,
                height=VIDEO_H,
                message="Video pipeline unavailable",
            )
        else:
            if self._tap is not None:
                frame = self._tap.latest_frame()
                if frame is not None:
                    self._compositor.set(frame)
            self._compositor.paint(
                canvas,
                0,
                0,
                palette=palette,
                frame=frame if frame is not None else self._compositor.latest(),
                width=PAGE_W,
                height=VIDEO_H,
                message=(
                    "Video link not available — waiting for stream"
                ),
            )

        # Metrics strip.
        self._draw_metrics(canvas, palette, dim=frame is None)

        # Top-left REC button.
        pulse = (time.monotonic() % 1.0) if self._recording else 0.0
        draw_rec_button(
            canvas,
            8,
            8,
            palette=palette,
            recording=self._recording,
            pulse_phase=pulse,
        )

        # Top-right camera chip (hidden when only one camera).
        draw_camera_chip(
            canvas,
            PAGE_W - 88,
            8,
            palette=palette,
            label=self._camera_label,
            count=self._camera_count,
        )

        # Detail HUD overlay sits above the live frame so the operator
        # can read decoder telemetry without losing the picture.
        if self._show_detail_hud:
            self._draw_detail_hud(canvas, palette)

        return canvas

    def _draw_metrics(
        self,
        canvas: Image.Image,
        palette,  # type: ignore[no-untyped-def]
        *,
        dim: bool,
    ) -> None:
        draw = ImageDraw.Draw(canvas)
        # Background plate so the metrics row reads as a separate band.
        draw.rectangle(
            (0, VIDEO_H, PAGE_W - 1, PAGE_H - 1),
            fill=palette.bg_secondary,
        )
        # Top divider so the eye separates picture from data.
        draw.line(
            (0, VIDEO_H, PAGE_W - 1, VIDEO_H),
            fill=palette.border_default,
        )
        label_font = p.font("sans_bold", 9)
        value_font = p.font("mono_regular", 11)
        col_w = PAGE_W // 3
        rows = [
            (
                "LATENCY",
                self._format_latency(self._metrics_cache.get("latency_ms")),
                "RSSI",
                self._format_rssi(self._metrics_cache.get("rssi_dbm")),
                "BITRATE",
                self._format_bitrate(self._metrics_cache.get("bitrate_kbps")),
            ),
            (
                "FEC DROPS",
                self._format_drops(self._metrics_cache.get("fec_drops")),
                "FPS",
                self._format_fps(),
                "RADIO",
                self._format_radio(
                    self._metrics_cache.get("channel"),
                    self._metrics_cache.get("mcs_index"),
                ),
            ),
        ]
        label_color = palette.text_tertiary if dim else palette.text_secondary
        value_color = palette.text_secondary if dim else palette.text_primary
        for r_idx, row in enumerate(rows):
            ry = VIDEO_H + 6 + r_idx * 30
            for c_idx in range(3):
                lx = c_idx * col_w + 12
                lbl = row[c_idx * 2]
                val = row[c_idx * 2 + 1]
                draw.text(
                    (lx, ry),
                    lbl,
                    fill=label_color,
                    font=label_font,
                )
                draw.text(
                    (lx, ry + 11),
                    val,
                    fill=value_color,
                    font=value_font,
                )

    def _draw_detail_hud(
        self,
        canvas: Image.Image,
        palette,  # type: ignore[no-untyped-def]
    ) -> None:
        # Translucent overlay rectangle. PIL has no native alpha
        # composite for a single rectangle without RGBA roundtrips, so
        # we paint a solid darkened panel that approximates 60% black.
        overlay = Image.new("RGB", (PAGE_W, VIDEO_H), (0, 0, 0))
        canvas.paste(
            Image.blend(canvas.crop((0, 0, PAGE_W, VIDEO_H)), overlay, 0.6),
            (0, 0),
        )
        draw = ImageDraw.Draw(canvas)
        title_font = p.font("sans_bold", 12)
        body_font = p.font("mono_regular", 11)
        decoder = "--"
        fps = 0.0
        frames_decoded = 0
        frames_dropped = 0
        pipeline_latency_ms: Any = None
        decode_cpu: Any = None
        if self._tap is not None:
            stats = self._tap.stats()
            decoder = stats.get("decoder_type") or "--"
            fps = float(stats.get("fps") or 0.0)
            frames_decoded = int(stats.get("frames_decoded") or 0)
            frames_dropped = int(stats.get("frames_dropped") or 0)
            pipeline_latency_ms = stats.get("pipeline_latency_ms")
            decode_cpu = stats.get("decode_cpu_percent")
        draw.text(
            (16, 12),
            "DECODER",
            fill=palette.accent_primary,
            font=title_font,
        )

        def _fmt_ms(value: Any) -> str:
            if isinstance(value, (int, float)):
                return f"{int(value)} ms"
            return "--"

        def _fmt_pct(value: Any) -> str:
            if isinstance(value, (int, float)):
                return f"{float(value):.0f} %"
            return "--"

        lines = [
            f"path     {decoder}",
            f"fps      {fps:.1f}",
            f"frames   {frames_decoded}",
            f"dropped  {frames_dropped}",
            f"bitrate  {self._format_bitrate(self._metrics_cache.get('bitrate_kbps'))}",
            f"pipe lat {_fmt_ms(pipeline_latency_ms)}",
            f"cpu      {_fmt_pct(decode_cpu)}",
        ]
        for i, ln in enumerate(lines):
            draw.text(
                (16, 30 + i * 14),
                ln,
                fill=palette.text_primary,
                font=body_font,
            )
        # Right side: bitrate sparkline + FEC histogram strip.
        if len(self._bitrate_history) >= 2:
            from ados.services.ui.dashboards.components.sparkline import (
                draw_sparkline,
            )

            spark_x = 260
            spark_y = 30
            spark_w = PAGE_W - spark_x - 24
            spark_h = 56
            draw.text(
                (spark_x, spark_y - 14),
                "BITRATE 60s",
                fill=palette.text_tertiary,
                font=p.font("sans_bold", 9),
            )
            draw_sparkline(
                canvas,
                spark_x,
                spark_y,
                spark_w,
                spark_h,
                list(self._bitrate_history),
                color=palette.accent_secondary,
            )
        # FEC histogram below the bitrate sparkline. Pulls the rolling
        # ``loss_percent`` history if the in-process LinkQualityMonitor
        # is reachable; otherwise renders a thin "no data" line.
        fec_history = self._collect_fec_history()
        if fec_history:
            from ados.services.ui.dashboards.components.sparkline import (
                draw_sparkline,
            )

            hist_x = 260
            hist_y = 102
            hist_w = PAGE_W - hist_x - 24
            hist_h = 42
            draw.text(
                (hist_x, hist_y - 14),
                "FEC LOSS 60s",
                fill=palette.text_tertiary,
                font=p.font("sans_bold", 9),
            )
            draw_sparkline(
                canvas,
                hist_x,
                hist_y,
                hist_w,
                hist_h,
                fec_history,
                color=palette.status_warning,
            )

    def _collect_fec_history(self) -> list[float]:
        """Pull a 60 s ``loss_percent`` slice from the in-process monitor.

        Returns an empty list when the agent app or monitor is not
        reachable (typical in unit tests). The detail HUD renders a
        graceful empty state in that case.
        """
        try:
            from ados.api.deps import get_agent_app

            wfb = get_agent_app().wfb_manager()
            if wfb is None:
                return []
            monitor = getattr(wfb, "monitor", None)
            if monitor is None:
                return []
            samples = monitor.get_history(seconds=60)
            return [float(s.loss_percent) for s in samples]
        except Exception:  # noqa: BLE001
            return []

    @staticmethod
    def _format_latency(value: Any) -> str:
        if isinstance(value, (int, float)):
            return f"{int(value)} ms"
        return "--"

    @staticmethod
    def _format_rssi(value: Any) -> str:
        if isinstance(value, (int, float)):
            return f"{int(value)} dBm"
        return "--"

    @staticmethod
    def _format_bitrate(value: Any) -> str:
        if isinstance(value, (int, float)):
            kbps = float(value)
            if kbps >= 1000:
                return f"{kbps / 1000:.1f} Mbps"
            return f"{kbps:.0f} kbps"
        return "--"

    @staticmethod
    def _format_drops(value: Any) -> str:
        # Tuple form: (lost, total) renders as "lost / total" so the
        # operator sees both the loss count and the denominator. Bare
        # int form falls through to the legacy "lost" display so older
        # cached values do not crash the renderer.
        if isinstance(value, tuple) and len(value) == 2:
            lost, total = value
            try:
                return f"{int(lost)} / {int(total)}"
            except (TypeError, ValueError):
                return "--"
        if isinstance(value, (int, float)):
            return str(int(value))
        return "--"

    def _format_fps(self) -> str:
        if self._tap is None:
            return "--"
        stats = self._tap.stats()
        fps = float(stats.get("fps") or 0.0)
        if fps <= 0:
            return "--"
        return f"{fps:.1f}"

    @staticmethod
    def _format_radio(channel: Any, mcs: Any) -> str:
        ch = str(int(channel)) if isinstance(channel, (int, float)) else "--"
        mc = f"MCS{int(mcs)}" if isinstance(mcs, (int, float)) else ""
        if mc:
            return f"ch{ch} {mc}"
        return f"ch{ch}"

    # ── hit zones + dispatch ───────────────────────────────────

    def hit_zones(self, ctx: PageContext) -> list[HitZone]:
        zones: list[HitZone] = [
            HitZone(id="video.rec_button", x=8, y=8, w=80, h=32),
        ]
        if self._camera_count > 1:
            zones.append(
                HitZone(id="video.cam_chip", x=PAGE_W - 88, y=8, w=80, h=32),
            )
        # Surface zone covers the rest of the video region. Painting
        # this zone last means the chip and rec button take precedence
        # in dispatch order; the navigator iterates and stops at first
        # contains() hit.
        zones.append(
            HitZone(id="video.surface", x=0, y=0, w=PAGE_W, h=VIDEO_H),
        )
        # Metrics strip is a no-op zone — it absorbs taps so they
        # don't leak into a swipe-up navigation gesture.
        zones.append(
            HitZone(
                id="video.metrics_strip",
                x=0,
                y=VIDEO_H,
                w=PAGE_W,
                h=METRICS_H,
            )
        )
        return zones

    async def on_touch(
        self,
        ctx: PageContext,
        zone: HitZone,
        gesture: TouchGesture,
    ) -> None:
        if gesture.kind != "tap":
            return
        if zone.id == "video.rec_button":
            await self._toggle_recording(ctx)
            return
        if zone.id == "video.cam_chip":
            await self._open_camera_picker(ctx)
            return
        if zone.id == "video.surface":
            self._show_detail_hud = not self._show_detail_hud
            ctx.logger.info(
                "video_detail_hud_toggled",
                visible=self._show_detail_hud,
            )

    async def _toggle_recording(self, ctx: PageContext) -> None:
        client = ctx.http
        if client is None:
            return
        endpoint = (
            "/api/video/record/stop"
            if self._recording
            else "/api/video/record/start"
        )
        try:
            r = await client.post(endpoint, timeout=2.0)
            status = getattr(r, "status_code", None)
            if status is not None and 200 <= status < 300:
                blob = r.json() if callable(getattr(r, "json", None)) else {}
                if isinstance(blob, dict) and "recording" in blob:
                    self._recording = bool(blob.get("recording"))
                else:
                    self._recording = not self._recording
                ctx.logger.info(
                    "video_recording_toggled",
                    recording=self._recording,
                )
            else:
                ctx.logger.warning(
                    "video_recording_toggle_rejected",
                    status=status,
                )
        except Exception as exc:  # noqa: BLE001
            ctx.logger.debug("video_recording_toggle_failed", error=str(exc))

    async def _open_camera_picker(self, ctx: PageContext) -> None:
        if self._camera_count <= 1:
            return
        options: list[tuple[str, str]] = []
        current: str | None = None
        for i, cam in enumerate(self._cameras):
            value = str(cam.get("device_path") or cam.get("id") or i)
            label = str(cam.get("label") or cam.get("name") or f"CAM {i + 1}")
            options.append((value, label))
            if cam.get("active"):
                current = value
        if not options:
            return

        async def _save(value: str) -> None:
            client = ctx.http
            if client is None:
                return
            try:
                await client.post(
                    "/api/video/camera/switch",
                    json={"role": "primary", "device_path": value},
                    timeout=2.0,
                )
            except Exception as exc:  # noqa: BLE001
                ctx.logger.debug("video_camera_switch_failed", error=str(exc))
            await self._refresh_metrics_once(ctx)

        await ctx.navigator.push_modal(
            EnumPickerModal(
                title="Camera",
                options=options,
                current=current,
                on_save=_save,
            ),
            ctx=ctx,
        )


def _write_tap_status(path: Any, blob: dict) -> None:
    """Atomic-write a JSON blob to ``path``.

    Helper for ``VideoPage._publish_tap_status``. Kept module-level so
    the import graph stays acyclic (the page should not depend on the
    cloud heartbeat module to write a sidecar file).
    """
    import json
    import os
    import tempfile
    from pathlib import Path as _Path

    target = _Path(str(path))
    try:
        target.parent.mkdir(parents=True, exist_ok=True)
        fd, tmp = tempfile.mkstemp(
            prefix=target.name + ".",
            suffix=".tmp",
            dir=str(target.parent),
        )
        with os.fdopen(fd, "w") as fh:
            json.dump(blob, fh, separators=(",", ":"))
            fh.flush()
            os.fsync(fh.fileno())
        os.replace(tmp, target)
    except OSError:
        try:
            os.unlink(tmp)  # type: ignore[name-defined]
        except (OSError, NameError):
            pass
