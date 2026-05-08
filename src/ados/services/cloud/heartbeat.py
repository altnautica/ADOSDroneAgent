"""Pure helpers for the cloud heartbeat payload.

The cloud subprocess (``ados.services.cloud.__main__``) builds and
ships the periodic status heartbeat that Mission Control reads to
populate the per-drone capability store. The payload-assembly closure
in ``__main__.py`` is tightly coupled to its own asyncio loop state
(pairing transition tracking, follow-up tick scheduling, the shared
``httpx`` client), but the field-by-field source helpers it folds in
are pure: they read local sidecar files, query the agent's REST
surface on localhost, and reduce the result to a simple dict-shaped
fragment.

This module owns those helpers. The ``__main__`` module imports the
helpers and the module-global ``_proc_cache`` from here so a single
psutil ``Process`` baseline is held across heartbeats. Keeping the
cache here (instead of duplicating it in ``__main__``) is what makes
``cpuPercent`` correct on the wire: psutil's ``cpu_percent(interval=0)``
returns the delta since the last call on the same Process instance,
so we have to keep that instance alive between heartbeat ticks.
"""

from __future__ import annotations

import socket
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from ados.core.paths import (
    DISPLAY_CONF_PATH,
    LCD_VIDEO_TAP_PATH,
    TOUCH_CALIB_PATH,
)


# PID → psutil.Process cache for CPU baseline. psutil's
# ``cpu_percent(interval=0)`` reports the delta since the previous
# call on the same Process instance, so we hold the instance alive
# across heartbeat iterations. Stale PIDs are pruned by
# ``get_services_status`` after each refresh.
_proc_cache: dict[int, object] = {}


def now_iso() -> str:
    """ISO 8601 timestamp in the local timezone, second-precision."""
    return (
        datetime.now(timezone.utc).astimezone().replace(microsecond=0).isoformat()
    )


def _display_name(conf: dict[str, str]) -> str:
    """Friendly label for the heartbeat. Maps known display ids to a name."""
    mapping = {
        "waveshare35a": 'Waveshare 3.5" SPI LCD',
    }
    display_id = conf.get("display_id", "")
    if display_id in mapping:
        return mapping[display_id]
    if display_id:
        return display_id
    return "Local display"


def collect_attached_display() -> dict | None:
    """Build a peripherals[] entry for the attached SPI LCD when present.

    Reads ``/etc/ados/display.conf`` (written by the LCD-overlay
    installer) and translates it into the canonical peripheral shape
    consumed by Mission Control's infer-capabilities pipeline.
    Returns ``None`` when no display is provisioned, so the caller can
    drop ``peripherals`` from the payload entirely on boards that
    have no LCD.
    """
    if not DISPLAY_CONF_PATH.exists():
        return None
    try:
        text = DISPLAY_CONF_PATH.read_text()
    except OSError:
        return None
    conf: dict[str, str] = {}
    for raw in text.splitlines():
        line = raw.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        k, _, v = line.partition("=")
        conf[k.strip()] = v.strip()
    if not conf:
        return None
    fb_path = conf.get("framebuffer_path", "/dev/fb1")
    fb_name = (
        Path(f"/sys/class/graphics/{Path(fb_path).name}/name").read_text().strip()
        if Path(f"/sys/class/graphics/{Path(fb_path).name}/name").exists()
        else ""
    )
    bound = bool(fb_name)
    has_touch = (conf.get("has_touch", "false").lower() == "true")
    return {
        "id": "local-display",
        "name": _display_name(conf),
        "category": "display",
        "type": "spi-lcd",
        "bus": conf.get("bus", "spi"),
        "address": fb_path,
        "rate_hz": 0,
        "status": "ok" if bound else "warning",
        "last_reading": now_iso(),
        "extra": {
            "controller": conf.get("controller", ""),
            "touch_chip": conf.get("touch_chip", ""),
            "has_touch": has_touch,
            "resolution": conf.get("resolution", ""),
            "rotation": int(conf.get("rotation", 0) or 0),
            "board": conf.get("board", ""),
            "display_id": conf.get("display_id", ""),
            "framebuffer_path": fb_path,
            "framebuffer_name_actual": fb_name,
            "framebuffer_name_expected": conf.get("framebuffer_name_expected", ""),
            "overlay_source": conf.get("overlay_source", ""),
            "overlay_ref": conf.get("overlay_ref", ""),
            "activated_via": conf.get("activated_via", ""),
            "bound": bound,
        },
    }


def get_services_status() -> list[dict]:
    """Query systemd for all ados-* service states + per-PID metrics."""
    import subprocess

    try:
        import psutil
    except ImportError:
        psutil = None

    svc_names = [
        "ados-supervisor", "ados-mavlink", "ados-api", "ados-cloud",
        "ados-health", "ados-video", "ados-wfb", "ados-scripting",
        "ados-ota", "ados-discovery",
    ]
    categories = {
        "ados-supervisor": "core", "ados-mavlink": "core",
        "ados-api": "core", "ados-cloud": "core", "ados-health": "core",
        "ados-video": "hardware", "ados-wfb": "hardware",
        "ados-scripting": "suite", "ados-ota": "ondemand",
        "ados-discovery": "ondemand",
    }
    services = []
    for name in svc_names:
        try:
            result = subprocess.run(
                ["systemctl", "is-active", name],
                capture_output=True, text=True, timeout=5,
            )
            raw = result.stdout.strip()
            state = "running" if raw == "active" else ("failed" if raw == "failed" else "stopped")
        except Exception:
            state = "stopped"

        pid = None
        cpu = 0.0
        mem = 0.0
        uptime_secs = 0
        if state == "running" and psutil:
            try:
                pid_result = subprocess.run(
                    ["systemctl", "show", "-p", "MainPID", "--value", name],
                    capture_output=True, text=True, timeout=5,
                )
                pid = int(pid_result.stdout.strip())
                if pid > 0:
                    # Reuse cached Process objects so cpu_percent() has a baseline
                    proc = _proc_cache.get(pid)
                    if proc is None:
                        proc = psutil.Process(pid)
                        _proc_cache[pid] = proc
                        proc.cpu_percent(interval=0)  # Prime the baseline
                    cpu = proc.cpu_percent(interval=0)
                    mem = proc.memory_info().rss / (1024 * 1024)
                    uptime_secs = int(time.time() - proc.create_time())
            except Exception:
                pass

        entry: dict = {
            "name": name,
            "status": state,
            "cpuPercent": round(cpu, 1),
            "memoryMb": round(mem, 1),
            "uptimeSeconds": uptime_secs,
            "category": categories.get(name, "core"),
        }
        # Only include PID if it's a real value (Convex rejects null for v.number())
        if pid and pid > 0:
            entry["pid"] = pid
        services.append(entry)

    # Clean stale PIDs from cache
    active_pids = {s.get("pid") for s in services if s.get("pid")}
    stale = [p for p in _proc_cache if p not in active_pids]
    for p in stale:
        _proc_cache.pop(p, None)

    # Fallback: if ALL services show stopped, try psutil process scanning
    # (agent may run as direct processes under ados-supervisor, not systemd units)
    all_stopped = all(s["status"] == "stopped" for s in services)
    if all_stopped and psutil:
        MODULE_TO_SERVICE = {
            "ados.services.mavlink": "ados-mavlink",
            "ados.services.api": "ados-api",
            "ados.services.cloud": "ados-cloud",
            "ados.services.health": "ados-health",
            "ados.services.video": "ados-video",
            "ados.services.network": "ados-wfb",
            "ados.services.scripting": "ados-scripting",
            "ados.services.ota": "ados-ota",
            "ados-supervisor": "ados-supervisor",
        }
        # Build lookup for quick update
        svc_lookup = {s["name"]: s for s in services}
        try:
            for proc in psutil.process_iter(["pid", "cmdline", "cpu_percent", "memory_info", "create_time"]):
                try:
                    cmdline = proc.info.get("cmdline") or []
                    cmdline_str = " ".join(cmdline)
                    matched_svc = None
                    for module_key, svc_name in MODULE_TO_SERVICE.items():
                        if module_key in cmdline_str:
                            matched_svc = svc_name
                            break
                    if matched_svc and matched_svc in svc_lookup:
                        entry = svc_lookup[matched_svc]
                        entry["status"] = "running"
                        entry["pid"] = proc.info["pid"]
                        entry["cpuPercent"] = round(proc.info.get("cpu_percent", 0.0), 1)
                        mem_info = proc.info.get("memory_info")
                        if mem_info:
                            entry["memoryMb"] = round(mem_info.rss / (1024 * 1024), 1)
                        ct = proc.info.get("create_time")
                        if ct:
                            entry["uptimeSeconds"] = int(time.time() - ct)
                except (psutil.NoSuchProcess, psutil.AccessDenied):
                    continue
        except Exception:
            pass

    return services


def read_touch_calib_status() -> dict:
    """Read the touch calibration file to determine calibrated/skipped state.

    Returns ``{"calibrated": bool, "skipped": bool}``. Used by the
    heartbeat enricher so the GCS Display sub-view can show whether
    the operator has calibrated the touchscreen, has explicitly
    skipped, or has not yet been prompted.
    """
    if not TOUCH_CALIB_PATH.exists():
        return {"calibrated": False, "skipped": False}
    try:
        import json as _json

        blob = _json.loads(TOUCH_CALIB_PATH.read_text())
    except (OSError, ValueError):
        return {"calibrated": False, "skipped": False}
    if not isinstance(blob, dict):
        return {"calibrated": False, "skipped": False}
    return {
        "calibrated": bool(blob.get("calibrated", False)),
        "skipped": bool(blob.get("skipped", False)),
    }


def read_display_rotation() -> int | None:
    """Return the configured display rotation, or ``None`` when unknown."""
    if not DISPLAY_CONF_PATH.exists():
        return None
    try:
        text = DISPLAY_CONF_PATH.read_text()
    except OSError:
        return None
    for raw in text.splitlines():
        line = raw.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        k, _, v = line.partition("=")
        if k.strip() == "rotation":
            try:
                value = int(v.strip())
            except ValueError:
                return None
            if value in (0, 90, 180, 270):
                return value
            return None
    return None


def _read_json_with_retry(
    path: Path, attempts: int = 3, delay_s: float = 0.005,
) -> dict | None:
    """Read a JSON sidecar file with a small retry on partial writes.

    The OLED service writes these sidecars atomically (tmpfile +
    rename), but a reader that catches the inode mid-rename can still
    see an empty file. Mirror the retry pattern used by the REST
    surface in ``api/routes/display.py::_read_lcd_state``: try up to
    ``attempts`` times with ``delay_s`` between attempts. Returns the
    decoded dict on success, ``None`` on persistent absence, empty,
    or malformed content.
    """
    if not path.exists():
        return None
    import json as _json

    for attempt in range(max(1, attempts)):
        try:
            text = path.read_text()
        except OSError:
            return None
        if not text.strip():
            if attempt < attempts - 1:
                time.sleep(delay_s)
                continue
            return None
        try:
            blob = _json.loads(text)
        except _json.JSONDecodeError:
            if attempt < attempts - 1:
                time.sleep(delay_s)
                continue
            return None
        if not isinstance(blob, dict):
            return None
        return blob
    return None


def read_lcd_state_blob() -> dict | None:
    """Read ``/run/ados/lcd-state.json`` with the standard retry loop.

    Used by the heartbeat builder to surface the SPI LCD's active
    page. The retry covers the rare race where the cloud subprocess
    catches the OLED service's atomic-write rename in flight and
    sees an empty file. Returns the decoded dict on success, ``None``
    when the file is genuinely absent / empty / malformed even after
    retries.
    """
    from ados.core.paths import LCD_STATE_PATH

    return _read_json_with_retry(LCD_STATE_PATH)


def read_lcd_video_tap() -> dict | None:
    """Read ``/run/ados/lcd-video-tap.json`` published by the OLED video page.

    The file may be absent (LCD service not running, video page never
    visited yet). Returns ``None`` in that case so the caller can omit
    the related heartbeat fields entirely instead of advertising
    ``False`` for "decoder active" on a board with no LCD.

    Uses the same retry loop as :func:`read_lcd_state_blob` so a
    reader that catches an in-flight atomic rename does not silently
    drop the decoder block from the heartbeat.
    """
    blob = _read_json_with_retry(LCD_VIDEO_TAP_PATH)
    if blob is None:
        return None
    # Tap snapshots older than 30 s are stale (operator left the
    # video page over half a minute ago, the metrics loop is no
    # longer ticking). Drop them so the GCS doesn't show frozen
    # decoder state.
    updated_at = blob.get("updated_at_ms")
    if isinstance(updated_at, (int, float)):
        age_ms = (time.time() * 1000) - float(updated_at)
        if age_ms > 30_000:
            return None
    return blob


def read_recent_touch() -> dict | None:
    """Return the most recent touch event published by the OLED service.

    The OLED service holds the ring buffer in process memory, so the
    cloud subprocess cannot read it directly. We hit the local API
    surface (port 8080) the same way the radio block fetcher does,
    with a tight 0.2 s budget.
    """
    try:
        import httpx

        with httpx.Client(timeout=0.2) as client:
            resp = client.get(
                "http://127.0.0.1:8080/api/v1/display/touches"
            )
            if resp.status_code != 200:
                return None
            data = resp.json()
            events = data.get("events") if isinstance(data, dict) else None
            if not isinstance(events, list) or not events:
                return None
            last = events[-1]
            if not isinstance(last, dict):
                return None
            return last
    except Exception:
        return None


def read_video_recording_state() -> bool | None:
    """Ask the local agent whether the recorder is currently active.

    Mirrors the precedent set by the radio block fetcher: the cloud
    subprocess does not own the video pipeline, so it queries the
    agent's REST surface on localhost. ``None`` means we couldn't
    tell (API unreachable, response malformed) — the heartbeat
    enricher then omits the field entirely.
    """
    try:
        import httpx

        with httpx.Client(timeout=0.2) as client:
            resp = client.get("http://127.0.0.1:8080/api/video")
            if resp.status_code != 200:
                return None
            data = resp.json()
            if not isinstance(data, dict):
                return None
            if "recording" in data:
                return bool(data["recording"])
            return None
    except Exception:
        return None


def build_display_enrichment(
    config: Any,
    *,
    has_attached_display: bool,
    local_ip: str,
    api_port: int,
) -> dict:
    """Return the optional display + UI fields the heartbeat will fold in.

    Each field is omitted when its source is unavailable, mirroring
    the existing pattern for ``temperature`` / ``peripherals``.
    Callers ``payload.update(enrich)`` so missing keys do not clobber
    previously-set heartbeat fields.
    """
    enrich: dict[str, Any] = {}

    # Theme (always read from config; defaults to "dark" if the block
    # is missing, which is the documented default).
    try:
        theme = getattr(getattr(config, "ui", None), "theme", "dark")
        if isinstance(theme, str) and theme in ("dark", "light"):
            enrich["uiTheme"] = theme
    except Exception:
        pass

    # Display-rooted fields are only meaningful when an LCD is bound.
    if has_attached_display:
        calib = read_touch_calib_status()
        # "calibrated" maps to whether the affine is on disk and
        # actively used. The skip marker is NOT calibrated.
        enrich["lcdTouchCalibrated"] = bool(calib.get("calibrated"))
        rotation = read_display_rotation()
        if rotation is not None:
            enrich["lcdRotation"] = rotation
        snapshot_url = (
            f"http://{local_ip}:{api_port}/api/v1/display/snapshot"
        )
        enrich["lcdSnapshotUrl"] = snapshot_url
        last_touch = read_recent_touch()
        if last_touch is not None:
            t_ms = last_touch.get("t")
            if isinstance(t_ms, (int, float)):
                enrich["lcdLastTouchAt"] = int(t_ms)
            kind = last_touch.get("kind")
            if isinstance(kind, str) and kind:
                enrich["lcdLastGesture"] = kind

    # Local-decoder + recording state. Sourced via the lcd-video-tap.json
    # snapshot for decoder + fps + active flag (which depends on the
    # OLED video page running), and a separate /api/video poll for the
    # recording flag (the recorder is pipeline-owned, independent of
    # the LCD page).
    tap = read_lcd_video_tap()
    if tap is not None:
        enrich["videoLocalDecoderActive"] = bool(tap.get("active"))
        decoder = tap.get("decoder")
        if isinstance(decoder, str) and decoder:
            enrich["videoLocalDecoderType"] = decoder
        fps = tap.get("fps")
        if isinstance(fps, (int, float)):
            enrich["videoLocalDecoderFps"] = float(fps)
    recording = read_video_recording_state()
    if recording is not None:
        enrich["videoRecording"] = bool(recording)
    return enrich


def get_local_ip() -> str:
    """Detect local IP via UDP socket probe."""
    try:
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        s.connect(("8.8.8.8", 80))
        ip = s.getsockname()[0]
        s.close()
        return ip
    except OSError:
        return "127.0.0.1"
