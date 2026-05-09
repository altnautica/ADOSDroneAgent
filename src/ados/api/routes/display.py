"""Display REST surface consumed by the Mission Control GCS.

The Display sub-view in the GCS lets a remote operator drive the same
SPI LCD that the operator on the bench is looking at: it polls
``/snapshot`` for a downsampled PNG of the current panel, ``/page`` to
read which page is open, ``POST /page`` to switch pages, and the
``/calibrate/*`` quintet to drive the 5-point touch wizard from the
browser. ``/touches`` returns a tail of recent touch events so the
remote operator can see which corner just got tapped.

Implementation notes:

* The calibration session is held in
  :mod:`ados.services.ui.touch.session`. Both the REST routes here and
  the on-LCD wizard mutate the same singleton — a remote ``/start``
  arms the wizard on the panel; a tap on the panel mirrors into the
  shared step counter so the GCS poll sees live progress.
* The snapshot endpoint reads the framebuffer device directly. The
  cloud service and the OLED service are separate processes; the API
  process cannot reach the OLED's PIL canvas in memory, but it can
  open and ``mmap`` the same kernel framebuffer at ``/dev/fb1``. We
  cache the resulting PNG for ~800 ms so a half-second of concurrent
  polls collapses into a single read.
* ``POST /page`` writes the requested page id to a JSON request file
  the OLED service watches. The handshake is one-way and idempotent:
  the OLED service unlinks the file after applying the request so a
  stale entry can never reapply on a future tick.

All routes are read- or session-scoped — none mutate config — so they
sit under the standard ``/api/v1`` prefix without elevated auth
beyond the usual API key middleware.
"""

from __future__ import annotations

import io
import json
import threading
import time
from pathlib import Path
from typing import Any

from fastapi import APIRouter, HTTPException, Query
from fastapi.responses import Response
from pydantic import BaseModel, Field

from ados.core.atomic import atomic_write_json
from ados.core.logging import get_logger
from ados.core.paths import (
    DISPLAY_CONF_PATH,
    LCD_PAGE_REQUEST_PATH,
    LCD_STATE_PATH,
    TOUCH_CALIB_PATH,
)
from ados.services.ui.display_conf import read_rotation
from ados.services.ui.touch.recent import recent_touches
from ados.services.ui.touch.session import (
    STEP_COUNT,
    TARGETS,
    get_session_registry,
)
from ados.services.ui.touch.transform import load as load_calib

log = get_logger("api.display")

router = APIRouter(prefix="/v1/display", tags=["display"])


# Page ids the navigator owns. POST /page validates against this set
# so a typo never hangs the OLED service waiting on an unknown route.
# `more` is kept in the set for backward compat — the More tab was
# replaced by `link_stats` in the bottom bar but the MorePage class is
# still importable and a stale GCS request that targets `more` should
# still resolve rather than 400.
_VALID_PAGE_IDS: frozenset[str] = frozenset(
    {"dashboard", "video", "settings", "more", "link_stats"}
)


# ── snapshot caching ────────────────────────────────────────────────

# Cache the rendered PNG for this many milliseconds. The Display
# sub-view polls at 1 Hz; concurrent renders from a second tab or a
# burst of GCS reloads collapse into the same cached payload.
_SNAPSHOT_CACHE_TTL_MS = 800

_snap_lock = threading.Lock()
# Keyed by (width, height) so two viewports requesting different
# downsamples don't fight each other for the same cache slot.
_snap_cache: dict[tuple[int, int], tuple[bytes, float]] = {}


# ── helpers ─────────────────────────────────────────────────────────


def _read_display_conf() -> dict[str, str]:
    if not DISPLAY_CONF_PATH.exists():
        return {}
    out: dict[str, str] = {}
    try:
        for raw in DISPLAY_CONF_PATH.read_text().splitlines():
            line = raw.strip()
            if not line or line.startswith("#") or "=" not in line:
                continue
            k, _, v = line.partition("=")
            out[k.strip()] = v.strip()
    except OSError:
        return {}
    return out


def _lcd_is_bound() -> bool:
    """Best-effort check: is an SPI LCD framebuffer present?"""
    conf = _read_display_conf()
    fb_path = conf.get("framebuffer_path", "")
    if not fb_path:
        return False
    return Path(fb_path).exists()


def _read_lcd_state() -> dict[str, Any]:
    """Read ``/run/ados/lcd-state.json`` with one retry on partial writes.

    The OLED service writes atomically, but a reader that catches the
    inode mid-rename can still see an empty file. A single retry after
    a 5 ms sleep covers the rare race.
    """
    for attempt in range(2):
        try:
            text = LCD_STATE_PATH.read_text()
        except OSError:
            return {"active_page": "dashboard", "modal_stack": []}
        if not text.strip():
            time.sleep(0.005)
            continue
        try:
            blob = json.loads(text)
        except json.JSONDecodeError:
            if attempt == 0:
                time.sleep(0.005)
                continue
            return {"active_page": "dashboard", "modal_stack": []}
        if not isinstance(blob, dict):
            return {"active_page": "dashboard", "modal_stack": []}
        return {
            "active_page": str(blob.get("active_page_id") or "dashboard"),
            "modal_stack": [
                str(x) for x in (blob.get("modal_stack") or [])
            ],
        }
    return {"active_page": "dashboard", "modal_stack": []}


def _read_framebuffer_image(conf: dict[str, str]) -> Any | None:
    """Decode the kernel framebuffer into a PIL image.

    Returns ``None`` when the framebuffer is missing, the geometry
    cannot be read, or PIL cannot be imported. Supports the three
    pixel formats fbtft / DRM expose for the SPI panel: RGB565 (16
    bpp), RGB24 (24 bpp), and xRGB32 (32 bpp). Other depths are
    rejected because the byte ordering would need to be guessed.
    """
    try:
        from PIL import Image
    except ImportError:
        return None

    fb_path = conf.get("framebuffer_path", "/dev/fb1")
    if not Path(fb_path).exists():
        return None

    fb_name = Path(fb_path).name
    sysfs = Path(f"/sys/class/graphics/{fb_name}")
    try:
        virtual = (sysfs / "virtual_size").read_text().strip()
        bpp_str = (sysfs / "bits_per_pixel").read_text().strip()
        xres_s, yres_s = virtual.split(",", 1)
        xres = int(xres_s)
        yres = int(yres_s)
        bpp = int(bpp_str)
    except (OSError, ValueError):
        return None
    if xres <= 0 or yres <= 0 or bpp not in (16, 24, 32):
        return None

    try:
        with open(fb_path, "rb") as fh:
            buf = fh.read(xres * yres * (bpp // 8))
    except OSError:
        return None
    if len(buf) < xres * yres * (bpp // 8):
        return None

    if bpp == 16:
        # RGB565 little-endian — unpack to RGB888.
        out = bytearray(xres * yres * 3)
        for i in range(xres * yres):
            lo = buf[2 * i]
            hi = buf[2 * i + 1]
            pix = lo | (hi << 8)
            r = (pix >> 11) & 0x1F
            g = (pix >> 5) & 0x3F
            b = pix & 0x1F
            out[3 * i + 0] = (r << 3) | (r >> 2)
            out[3 * i + 1] = (g << 2) | (g >> 4)
            out[3 * i + 2] = (b << 3) | (b >> 2)
        img = Image.frombytes("RGB", (xres, yres), bytes(out))
    elif bpp == 24:
        img = Image.frombytes("RGB", (xres, yres), buf)
    else:  # 32 bpp xRGB; bytes are B,G,R,X
        out = bytearray(xres * yres * 3)
        for i in range(xres * yres):
            out[3 * i + 0] = buf[4 * i + 2]  # R
            out[3 * i + 1] = buf[4 * i + 1]  # G
            out[3 * i + 2] = buf[4 * i + 0]  # B
        img = Image.frombytes("RGB", (xres, yres), bytes(out))
    return img


def _render_snapshot_png(width: int, height: int) -> bytes | None:
    """Read fb1, downsample, encode PNG. Returns ``None`` when no LCD."""
    conf = _read_display_conf()
    img = _read_framebuffer_image(conf)
    if img is None:
        return None
    if (img.width, img.height) != (width, height):
        try:
            from PIL import Image as _PILImage  # noqa: F401
        except ImportError:
            return None
        img = img.resize((width, height))
    buf = io.BytesIO()
    img.save(buf, format="PNG")
    return buf.getvalue()


def _cached_snapshot(width: int, height: int) -> bytes | None:
    """Return a cached snapshot, rendering a new one when stale."""
    key = (width, height)
    now_ms = time.monotonic() * 1000
    with _snap_lock:
        cached = _snap_cache.get(key)
        if cached is not None and (now_ms - cached[1]) < _SNAPSHOT_CACHE_TTL_MS:
            return cached[0]
    payload = _render_snapshot_png(width, height)
    if payload is None:
        return None
    with _snap_lock:
        _snap_cache[key] = (payload, now_ms)
    return payload


# ── request models ──────────────────────────────────────────────────


class CalibrateSampleBody(BaseModel):
    """Body of ``POST /calibrate/sample``."""

    step: int = Field(..., ge=0, le=STEP_COUNT - 1)
    x_raw: int = Field(..., ge=0, le=4095)
    y_raw: int = Field(..., ge=0, le=4095)


class PageSetBody(BaseModel):
    """Body of ``POST /page``."""

    page: str = Field(..., min_length=1, max_length=32)


# ── routes: calibrate ───────────────────────────────────────────────


@router.post("/calibrate/start")
async def post_calibrate_start() -> dict[str, Any]:
    """Arm the wizard. Returns the target list and step counter.

    The OLED service watches the session generation counter on every
    render tick; the next tick after this call will see ``in_progress``
    True with a fresh generation and engage calibrate mode on the
    panel.
    """
    registry = get_session_registry()
    snap = registry.start()
    job_id = f"cal-{int((snap.started_at or 0) * 1000)}-{snap.generation}"
    return {
        "job_id": job_id,
        "target_count": STEP_COUNT,
        "current_step": snap.current_step,
        "targets": [
            {"idx": i, "x": tx, "y": ty}
            for i, (tx, ty) in enumerate(TARGETS)
        ],
    }


@router.post("/calibrate/sample")
async def post_calibrate_sample(body: CalibrateSampleBody) -> dict[str, Any]:
    """Record a sample for the wizard."""
    registry = get_session_registry()
    accepted, next_step, complete = registry.submit_sample(
        body.step, body.x_raw, body.y_raw,
    )
    return {
        "accepted": accepted,
        "next_step": next_step,
        "complete": complete,
    }


@router.post("/calibrate/save")
async def post_calibrate_save() -> dict[str, Any]:
    """Solve the affine and persist. Reports the residual on success.

    On rejection (RMS over the limit) the file is left alone so the
    operator can re-tap the same five targets without restarting the
    wizard from scratch — the GCS surfaces the residual + an explicit
    "Retry" button that maps to a fresh ``/start``.
    """
    registry = get_session_registry()
    rotation = read_rotation()
    ok, rms, error = registry.save(rotation=rotation)
    if not ok:
        # Per-error-shape message for the GCS dialog. Distinguish
        # "the operator must re-tap" from "we never had enough samples
        # to fit at all".
        return {
            "ok": False,
            "rms_residual_px": rms,
            "error": error or "save_failed",
        }
    return {"ok": True, "rms_residual_px": rms}


@router.post("/calibrate/skip")
async def post_calibrate_skip() -> dict[str, Any]:
    """Persist the skip marker so the wizard does not auto-launch."""
    registry = get_session_registry()
    ok = registry.skip()
    return {"ok": ok}


@router.get("/calibrate/status")
async def get_calibrate_status() -> dict[str, Any]:
    """Live calibration state for the GCS dialog poll.

    ``calibrated`` reflects the on-disk state (after ``save()`` flips
    in_progress to False). ``in_progress`` reflects the live wizard
    session. ``current_step`` and ``rms_residual_px`` are surfaced for
    the GCS progress card.
    """
    registry = get_session_registry()
    snap = registry.snapshot()
    on_disk = load_calib(TOUCH_CALIB_PATH)
    payload: dict[str, Any] = {
        "calibrated": on_disk is not None,
        "in_progress": snap.in_progress,
    }
    if snap.in_progress:
        payload["current_step"] = snap.current_step
    if snap.rms_residual_px is not None:
        payload["rms_residual_px"] = snap.rms_residual_px
    return payload


# ── routes: snapshot / page / touches ───────────────────────────────


@router.get("/snapshot")
async def get_snapshot(
    width: int = Query(240, ge=64, le=480),
    height: int = Query(160, ge=64, le=320),
) -> Response:
    """Return a PNG of the current LCD framebuffer.

    Default geometry (240x160) matches the Display sub-view's card
    thumbnail; the `width`/`height` query params let the GCS request
    a larger preview when the modal opens. The PNG is cached for
    ~800 ms server-side; the response carries ``Cache-Control:
    max-age=1`` so the browser also collapses near-duplicate requests
    on the network layer.
    """
    if not _lcd_is_bound():
        raise HTTPException(
            status_code=404,
            detail="no_lcd_bound",
        )
    payload = _cached_snapshot(width, height)
    if payload is None:
        raise HTTPException(
            status_code=503,
            detail="framebuffer_unreadable",
        )
    return Response(
        content=payload,
        media_type="image/png",
        headers={"Cache-Control": "max-age=1"},
    )


@router.get("/page")
async def get_page() -> dict[str, Any]:
    """Return the active page id + modal stack.

    Reads ``/run/ados/lcd-state.json`` which the navigator persists on
    every transition. The endpoint never raises; if the OLED service
    is down or the file is absent the response defaults to the
    dashboard so the GCS does not flicker on a brief outage.
    """
    return _read_lcd_state()


@router.post("/page")
async def post_page(body: PageSetBody) -> dict[str, Any]:
    """Request a remote page switch via a watch file.

    The OLED service polls ``/run/ados/lcd-page-request.json`` on
    each render tick. When the file appears, the navigator routes to
    the requested page and unlinks the file. Validation is strict:
    unknown ids return 400 so a typo never hangs the watcher.
    """
    page_id = body.page.strip()
    if page_id not in _VALID_PAGE_IDS:
        raise HTTPException(
            status_code=400,
            detail={
                "ok": False,
                "error": "unknown_page",
                "page": page_id,
                "valid": sorted(_VALID_PAGE_IDS),
            },
        )
    blob = {"page": page_id, "requested_at_ms": int(time.time() * 1000)}
    try:
        atomic_write_json(LCD_PAGE_REQUEST_PATH, blob)
    except OSError as exc:
        log.warning("lcd_page_request_write_failed", error=str(exc))
        raise HTTPException(
            status_code=500,
            detail="page_request_persist_failed",
        ) from exc
    return {"ok": True, "active_page": page_id}


@router.get("/touches")
async def get_touches(
    since_ms: int = Query(0, ge=0),
) -> dict[str, Any]:
    """Return the tail of recent touch events.

    The ring buffer holds the last 32 events. ``since_ms`` is the
    millisecond timestamp the GCS last saw; the route returns only
    events newer than that so a 1 Hz poll never re-renders the same
    event twice.
    """
    return {"events": recent_touches(since_ms=since_ms)}


# Re-exports kept here so `from ados.api.routes import display` works
# with the install-time API surface contract (server.py imports the
# module to register router; no symbol re-export needed).
__all__ = ["router"]
