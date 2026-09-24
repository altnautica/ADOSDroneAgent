"""Display REST surface consumed by the Mission Control GCS.

The Display sub-view in the GCS lets a remote operator see and drive the
panel the operator on the bench is looking at: it polls ``/snapshot`` for a
PNG of the current frame, ``/page`` to read which page is open, ``POST /page``
to switch pages, and ``/calibrate/{start,status}`` to launch the touch
calibration wizard.

Implementation notes:

* The panel UI, including the calibration wizard, is the native display
  service (``ados-display``). ``POST /calibrate/start`` drops the one-shot
  ``/run/ados/recalibrate.flag`` it consumes on its ~1 Hz loop; the wizard
  then runs on the panel, where the operator taps the crosshairs. The fit is
  written to ``/etc/ados/touch.calib``, which ``/calibrate/status`` reads.
* The snapshot endpoint serves the PNG ``ados-display`` drops at
  ``/run/ados/lcd-snapshot.png`` after each render — the exact frame on the
  panel. Only when that file is missing or stale does it read the kernel
  framebuffer directly and encode a PNG with the standard library. The result
  is cached for ~800 ms so a half-second of concurrent polls collapses into
  one read.
* ``POST /page`` writes the requested page id to a JSON request file the
  display service consumes and unlinks.
"""

from __future__ import annotations

import asyncio
import json
import threading
import time
from pathlib import Path
from typing import Any

from fastapi import APIRouter, HTTPException, Query
from fastapi.responses import Response
from pydantic import BaseModel, Field

from ados.api.routes._lcd_png import render_framebuffer_png
from ados.core.atomic import atomic_write_json
from ados.core.logging import get_logger
from ados.core.paths import (
    ADOS_RUN_DIR,
    DISPLAY_CONF_PATH,
    LCD_PAGE_REQUEST_PATH,
    LCD_SNAPSHOT_PATH,
    LCD_STATE_PATH,
    TOUCH_CALIB_PATH,
)
from ados.services.ui.touch.transform import load as load_calib

log = get_logger("api.display")

router = APIRouter(prefix="/v1/display", tags=["display"])


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


def _resolve_fb_path(conf: dict[str, str]) -> str | None:
    """Return the ``/dev/fbN`` node the SPI LCD is actually bound to.

    The overlay installer records ``framebuffer_path`` (commonly
    ``/dev/fb1``), but the kernel can assign the SPI LCD a different
    node than expected: when the DRM primary is disabled it does not
    claim ``/dev/fb0``, so the fbtft panel lands on ``fb0`` instead.
    Trust the configured path only when it exists and reports the
    expected driver; otherwise scan ``/sys/class/graphics/*`` for the
    framebuffer whose driver name matches ``framebuffer_name_expected``.
    This mirrors the resolution the OLED renderer's ``probe()`` performs
    so the remote snapshot and the on-panel UI agree on the device.
    """
    expected = (conf.get("framebuffer_name_expected") or "fb_ili9486").strip()
    candidates: list[str] = []
    configured = conf.get("framebuffer_path", "")
    if configured and Path(configured).exists():
        candidates.append(configured)
    sys_glob = Path("/sys/class/graphics")
    if sys_glob.exists():
        for entry in sorted(sys_glob.iterdir()):
            dev = f"/dev/{entry.name}"
            if (
                entry.name.startswith("fb")
                and dev not in candidates
                and Path(dev).exists()
            ):
                candidates.append(dev)
    for dev in candidates:
        if not expected:
            return dev
        try:
            name = (
                Path("/sys/class/graphics") / Path(dev).name / "name"
            ).read_text().strip()
        except OSError:
            continue
        if expected in name:
            return dev
    return None


def _load_lcd_state_blob() -> dict[str, Any] | None:
    """Read ``/run/ados/lcd-state.json`` with one retry on partial writes.

    The OLED service writes atomically, but a reader that catches the
    inode mid-rename can still see an empty file. A single retry after
    a 5 ms sleep covers the rare race. ``None`` when the file is absent,
    unreadable, or not a JSON object.
    """
    for attempt in range(2):
        try:
            text = LCD_STATE_PATH.read_text()
        except OSError:
            return None
        if not text.strip():
            time.sleep(0.005)
            continue
        try:
            blob = json.loads(text)
        except json.JSONDecodeError:
            if attempt == 0:
                time.sleep(0.005)
                continue
            return None
        return blob if isinstance(blob, dict) else None
    return None


def _read_lcd_state() -> dict[str, Any]:
    """The active page id and modal stack the navigator persisted.

    ``available`` is false and ``active_page`` null when no display service
    has published state: an absent panel has no active page, and reporting
    the dashboard there would show a page nothing is rendering.
    """
    blob = _load_lcd_state_blob()
    if blob is None:
        return {"available": False, "active_page": None, "modal_stack": []}
    active = blob.get("active_page_id")
    return {
        "available": True,
        "active_page": str(active) if active else None,
        "modal_stack": [
            str(x) for x in (blob.get("modal_stack") or [])
        ],
    }


def _registered_page_ids() -> frozenset[str]:
    """The route ids the display navigator registered.

    The navigator rewrites ``route_ids`` in ``lcd-state.json`` at every
    start, so the set is exactly the pages the running build can land on
    (the reserved plugin page and the channel-hops tab included). Empty
    when no display service has published it.
    """
    blob = _load_lcd_state_blob()
    ids = blob.get("route_ids") if blob else None
    if not isinstance(ids, list):
        return frozenset()
    return frozenset(x for x in ids if isinstance(x, str) and x)


# The Rust writer refreshes the snapshot PNG at ~1 Hz. Accept a file up
# to this old as live; past it the writer has likely stopped (the legacy
# fallback UI is running, or the daemon is down) and we read the
# framebuffer directly instead so the preview never goes stale.
_RUST_SNAPSHOT_MAX_AGE_S = 5.0


def _read_rust_snapshot() -> bytes | None:
    """Return the native writer's snapshot PNG when it is fresh.

    ``ados-display`` rewrites ``/run/ados/lcd-snapshot.png`` after each
    render. This is the exact frame on the panel, so it is the preferred
    source. Returns ``None`` when the file is absent, stale, or empty so
    the caller can fall back to a direct framebuffer read.
    """
    try:
        st = LCD_SNAPSHOT_PATH.stat()
    except OSError:
        return None
    if (time.time() - st.st_mtime) > _RUST_SNAPSHOT_MAX_AGE_S:
        return None
    try:
        data = LCD_SNAPSHOT_PATH.read_bytes()
    except OSError:
        return None
    return data or None


def _render_snapshot_png(width: int, height: int) -> bytes | None:
    """Return a PNG of the live panel, or ``None`` when there is none to show.

    Prefers the native writer's fresh snapshot (the exact panel frame), whatever
    kind of display it drives. Falls back to reading the SPI framebuffer and
    encoding a PNG with the standard library (no Pillow) only when the writer
    has not produced a recent frame. ``width`` / ``height`` are advisory — the
    panel is small and the GCS scales the image client-side.
    """
    rust = _read_rust_snapshot()
    if rust is not None:
        return rust

    fb_path = _resolve_fb_path(_read_display_conf())
    if fb_path is None:
        return None
    return render_framebuffer_png(fb_path)


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


class PageSetBody(BaseModel):
    """Body of ``POST /page``."""

    page: str = Field(..., min_length=1, max_length=32)


# ── routes: calibrate ───────────────────────────────────────────────

#: The one-shot request the native display service consumes on its ~1 Hz loop
#: to launch the on-panel calibration wizard, calibrated or not.
RECALIBRATE_FLAG_PATH = ADOS_RUN_DIR / "recalibrate.flag"

#: The crosshair count of the native wizard (a 3x3 grid).
CALIBRATION_TARGET_COUNT = 9


def arm_touch_recalibration() -> str | None:
    """Drop the recalibrate flag. Returns an error string, or None on success."""
    try:
        RECALIBRATE_FLAG_PATH.parent.mkdir(parents=True, exist_ok=True)
        RECALIBRATE_FLAG_PATH.write_text("1\n")
    except OSError as exc:
        return str(exc)
    return None


@router.post("/calibrate/start")
async def post_calibrate_start() -> dict[str, Any]:
    """Ask the panel to launch its calibration wizard.

    The wizard runs on the panel, where the operator taps the crosshairs; this
    only queues the request. There is no remote step counter: the result shows
    up in ``/calibrate/status`` as ``calibrated`` once the fit is saved.
    """
    error = arm_touch_recalibration()
    if error is not None:
        log.warning("recalibrate_flag_write_failed", error=error)
        raise HTTPException(status_code=500, detail="calibration_request_failed")
    return {"requested": True, "target_count": CALIBRATION_TARGET_COUNT}


@router.get("/calibrate/status")
async def get_calibrate_status() -> dict[str, Any]:
    """Calibration state for the GCS dialog poll.

    ``calibrated`` is the on-disk fit. ``requested`` is true while a start
    request is queued and the display service has not consumed it yet (it
    stays true when no display service is running to consume it).
    """
    return {
        "calibrated": load_calib(TOUCH_CALIB_PATH) is not None,
        "requested": RECALIBRATE_FLAG_PATH.exists(),
    }


# ── routes: snapshot / page ───────────────────────────────


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
    payload = await asyncio.to_thread(_cached_snapshot, width, height)
    if payload is None:
        # Neither a fresh native frame nor a readable SPI framebuffer.
        raise HTTPException(status_code=404, detail="no_lcd_bound")
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
    the requested page and unlinks the file. Validation is strict: the
    id must be one the running navigator registered (it publishes the
    list in ``lcd-state.json``), so a typo never hangs the watcher, and
    no request is queued while no display service has published one.
    """
    page_id = body.page.strip()
    valid = _registered_page_ids()
    if not valid:
        raise HTTPException(
            status_code=503,
            detail={
                "ok": False,
                "error": "display_not_running",
                "page": page_id,
            },
        )
    if page_id not in valid:
        raise HTTPException(
            status_code=400,
            detail={
                "ok": False,
                "error": "unknown_page",
                "page": page_id,
                "valid": sorted(valid),
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


# Re-exports kept here so `from ados.api.routes import display` works
# with the install-time API surface contract (server.py imports the
# module to register router; no symbol re-export needed).
__all__ = ["router"]
