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
    AIR_PIPELINE_STATS_PATH,
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
        "ados-scripting": "ondemand", "ados-ota": "ondemand",
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


def read_recent_touch(*, api_key: str | None = None) -> dict | None:
    """Return the most recent touch event published by the OLED service.

    The OLED service holds the ring buffer in process memory, so the
    cloud subprocess cannot read it directly. We hit the local API
    surface (port 8080) the same way the radio block fetcher does,
    with a tight 0.2 s budget.

    When the agent is paired the API requires X-ADOS-Key; callers must
    pass the agent's ``pairing.api_key`` or get a 401.
    """
    try:
        import httpx

        headers = {"X-ADOS-Key": api_key} if api_key else None
        with httpx.Client(timeout=0.2) as client:
            resp = client.get(
                "http://127.0.0.1:8080/api/v1/display/touches",
                headers=headers,
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


def read_air_pipeline_state() -> dict | None:
    """Read ``/run/ados/air-pipeline.json`` published by the AirPipeline.

    Returns ``None`` when the file is absent (legacy bash air pipeline
    in force, or the air-side service has not booted yet) so the
    heartbeat enricher can omit the related fields entirely.
    """
    blob = _read_json_with_retry(AIR_PIPELINE_STATS_PATH)
    if blob is None:
        return None
    # Drop snapshots older than 15 s — the air pipeline stats publisher
    # ticks at 1 Hz so a 15 s gap means the pipeline has crashed and
    # we should not surface stale state to the GCS.
    updated_at = blob.get("updated_at_ms")
    if isinstance(updated_at, (int, float)):
        age_ms = (time.time() * 1000) - float(updated_at)
        if age_ms > 15_000:
            return None
    return blob


def read_video_recording_state(*, api_key: str | None = None) -> bool | None:
    """Ask the local agent whether the recorder is currently active.

    Mirrors the precedent set by the radio block fetcher: the cloud
    subprocess does not own the video pipeline, so it queries the
    agent's REST surface on localhost. ``None`` means we couldn't
    tell (API unreachable, response malformed) — the heartbeat
    enricher then omits the field entirely.

    When the agent is paired the API requires X-ADOS-Key; callers must
    pass the agent's ``pairing.api_key`` or get a 401.
    """
    try:
        import httpx

        headers = {"X-ADOS-Key": api_key} if api_key else None
        with httpx.Client(timeout=0.2) as client:
            resp = client.get(
                "http://127.0.0.1:8080/api/video",
                headers=headers,
            )
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


def read_gs_video_state(*, api_key: str | None = None) -> bool | None:
    """Return whether a ground station is re-streaming received video.

    A ground station receives the drone's H.264 over the radio, decodes
    it, and republishes it on the local WHEP endpoint at port 8889. This
    asks the agent's ``/api/wfb`` surface whether frames are actually
    arriving so the heartbeat only advertises a stream the GCS can play.

    Liveness, not process-liveness: the receive link reports ``state ==
    "connected"`` only once it has decoded packets, and the same view
    flips to ``"stale"`` when the stats file stops updating. Gating on
    ``connected`` + a positive packet count means a starved or dead
    receiver is reported as not-streaming rather than a dead WHEP link.

    Returns ``True`` (frames flowing), ``False`` (receiver up but no
    frames / stale), or ``None`` (couldn't tell — API unreachable or
    malformed), in which case the caller falls back to not-streaming.

    When the agent is paired the API requires X-ADOS-Key; callers must
    pass the agent's ``pairing.api_key`` or get a 401.
    """
    try:
        import httpx

        headers = {"X-ADOS-Key": api_key} if api_key else None
        with httpx.Client(timeout=0.2) as client:
            resp = client.get(
                "http://127.0.0.1:8080/api/wfb",
                headers=headers,
            )
            if resp.status_code != 200:
                return None
            data = resp.json()
            if not isinstance(data, dict):
                return None
            state = data.get("state")
            packets = data.get("packets_received")
            if state != "connected":
                return False
            return isinstance(packets, (int, float)) and packets > 0
    except Exception:
        return None


def build_display_enrichment(
    config: Any,
    *,
    has_attached_display: bool,
    local_ip: str,
    api_port: int,
    api_key: str | None = None,
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
        last_touch = read_recent_touch(api_key=api_key)
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
    recording = read_video_recording_state(api_key=api_key)
    if recording is not None:
        enrich["videoRecording"] = bool(recording)
    # Phase 13: air-side pipeline flavor + encoder identity so the GCS
    # can render a "GST" pill on the drone card and surface the chosen
    # encoder (HW vs SW) on the Configure tab. Absent when the legacy
    # bash air pipeline owns the stream.
    air = read_air_pipeline_state()
    if air is not None:
        enrich["videoPipelineFlavor"] = "gst-native"
        encoder_name = air.get("encoder_name")
        if isinstance(encoder_name, str) and encoder_name:
            enrich["videoEncoderName"] = encoder_name
        hw_accel = air.get("encoder_hw_accel")
        if isinstance(hw_accel, bool):
            enrich["videoEncoderHwAccel"] = hw_accel
        camera_source = air.get("camera_source")
        if isinstance(camera_source, str) and camera_source:
            enrich["videoCameraSource"] = camera_source
        state = air.get("pipeline_state")
        if isinstance(state, str) and state:
            enrich["videoPipelineState"] = state
    return enrich


def build_can_buses_enrichment(
    param_cache_path: Path | str | None = None,
) -> dict:
    """Return a ``{"canBuses": [...]}`` fragment from the on-disk param cache.

    Reads the persisted parameter cache (default
    ``/var/lib/ados/params.json``) and surfaces the FC's per-port CAN
    configuration so Mission Control can render the active CAN buses
    on the drone card. Each bus entry carries:

    * ``port`` — 1 or 2, matching the FC parameter port suffix.
    * ``driver`` — ``CAN_Pn_DRIVER`` (0 = disabled, 1 = first driver, ...).
    * ``bitrate`` — ``CAN_Pn_BITRATE`` in bits per second.
    * ``protocol`` — ``CAN_Dn_PROTOCOL`` selected on the matching
      driver slot (1 = DroneCAN, other vendor protocols per FC docs).

    A port is included only if at least one of its four parameters is
    present in the cache. The block is omitted from the heartbeat
    entirely when no CAN params are cached yet, so older Mission
    Control builds that don't read this field see no change.

    Heartbeat-correctness contract: this helper does file I/O but
    never blocks on a parameter load. If the cache file is missing,
    empty, or malformed the helper returns ``{}`` and the heartbeat
    builder folds nothing in.
    """
    if param_cache_path is None:
        from ados.services.mavlink.param_cache import DEFAULT_CACHE_PATH
        param_cache_path = DEFAULT_CACHE_PATH
    path = Path(param_cache_path)
    if not path.is_file():
        return {}
    try:
        import json as _json
        raw = _json.loads(path.read_text())
    except (OSError, ValueError):
        return {}
    if not isinstance(raw, dict):
        return {}

    def _read_value(name: str) -> float | None:
        entry = raw.get(name)
        if isinstance(entry, dict) and "value" in entry:
            try:
                return float(entry["value"])
            except (TypeError, ValueError):
                return None
        return None

    buses: list[dict[str, int]] = []
    for port in (1, 2):
        driver = _read_value(f"CAN_P{port}_DRIVER")
        bitrate = _read_value(f"CAN_P{port}_BITRATE")
        protocol = _read_value(f"CAN_D{port}_PROTOCOL")
        # Skip ports where no CAN param has been observed yet. This
        # keeps the block empty during the param-load warmup window
        # rather than reporting a bogus all-zero bus.
        if driver is None and bitrate is None and protocol is None:
            continue
        buses.append({
            "port": port,
            "driver": int(driver) if driver is not None else 0,
            "bitrate": int(bitrate) if bitrate is not None else 0,
            "protocol": int(protocol) if protocol is not None else 0,
        })
    if not buses:
        return {}
    return {"canBuses": buses}


def build_display_type_enrichment(config: Any) -> dict:
    """Resolve the effective local-display primary path for the heartbeat.

    Returns ``{"displayType": "hdmi" | "lcd" | "none"}`` so the GCS can
    surface which renderer actually owns the panel on each ground
    station. When ``ground_station.display.type`` is anything other
    than ``"auto"`` the operator's selection is forwarded verbatim.
    Under ``"auto"`` the helper probes ``/dev/dri/card0`` for HDMI and
    ``/etc/ados/display.conf`` for a provisioned SPI LCD, picking
    HDMI if both are present and ``"none"`` if neither is.

    Returns an empty dict only if the helper cannot determine anything
    at all (defensive — keeps the heartbeat from carrying a misleading
    field). The caller does ``payload.update(...)``.
    """
    try:
        configured = getattr(
            getattr(config, "ground_station", None), "display", None
        )
        configured_type = getattr(configured, "type", "auto")
    except Exception:
        configured_type = "auto"

    if configured_type in ("hdmi", "lcd", "none"):
        return {"displayType": configured_type}

    # "auto": probe both renderers and prefer HDMI when both are wired.
    try:
        from ados.services.kiosk.kiosk_service import hdmi_present
        hdmi = bool(hdmi_present())
    except Exception:
        hdmi = False

    if hdmi:
        return {"displayType": "hdmi"}

    lcd = collect_attached_display() is not None
    if lcd:
        return {"displayType": "lcd"}

    return {"displayType": "none"}


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
