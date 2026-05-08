"""Standalone cloud relay service.

Handles:
- Pairing beacon (when unpaired): POSTs pairing code to Convex every 30s
- MQTT telemetry publishing (when paired): 2Hz to MQTT broker
- Convex HTTP heartbeat (when paired): full status every 5s
- Cloud command polling (when paired): checks for pending commands every 5s

Reads vehicle state from the state IPC socket.

Run: python -m ados.services.cloud
"""

from __future__ import annotations

import asyncio
import signal
import socket
import sys
import time
from collections import deque
from typing import Any

import structlog

from ados import __version__
from ados.core.config import load_config
from ados.core.ipc import StateIPCClient
from ados.core.logging import configure_logging
from ados.core.paths import (
    DISPLAY_CONF_PATH,
    HEALTH_JSON,
    LCD_VIDEO_TAP_PATH,
    SCRIPTS_DIR,
    SUITES_DIR,
    TOUCH_CALIB_PATH,
)


_proc_cache: dict[int, object] = {}  # PID → psutil.Process cache for CPU baseline


def _collect_attached_display() -> dict | None:
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
        "last_reading": _now_iso(),
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


def _now_iso() -> str:
    from datetime import datetime, timezone

    return (
        datetime.now(timezone.utc).astimezone().replace(microsecond=0).isoformat()
    )


# Pathlib import is used by _collect_attached_display + _display_name; keep
# the deferred import at use-time below to avoid a top-level dependency on
# pathlib for modules that only care about the heartbeat shape.
from pathlib import Path  # noqa: E402


def _get_services_status() -> list[dict]:
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


def _read_touch_calib_status() -> dict:
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


def _read_display_rotation() -> int | None:
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


def _read_lcd_state_blob() -> dict | None:
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


def _read_lcd_video_tap() -> dict | None:
    """Read ``/run/ados/lcd-video-tap.json`` published by the OLED video page.

    The file may be absent (LCD service not running, video page never
    visited yet). Returns ``None`` in that case so the caller can omit
    the related heartbeat fields entirely instead of advertising
    ``False`` for "decoder active" on a board with no LCD.

    Uses the same retry loop as :func:`_read_lcd_state_blob` so a
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


def _read_recent_touch() -> dict | None:
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


def _read_video_recording_state() -> bool | None:
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


def _build_display_enrichment(
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
        calib = _read_touch_calib_status()
        # "calibrated" maps to whether the affine is on disk and
        # actively used. The skip marker is NOT calibrated.
        enrich["lcdTouchCalibrated"] = bool(calib.get("calibrated"))
        rotation = _read_display_rotation()
        if rotation is not None:
            enrich["lcdRotation"] = rotation
        snapshot_url = (
            f"http://{local_ip}:{api_port}/api/v1/display/snapshot"
        )
        enrich["lcdSnapshotUrl"] = snapshot_url
        last_touch = _read_recent_touch()
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
    tap = _read_lcd_video_tap()
    if tap is not None:
        enrich["videoLocalDecoderActive"] = bool(tap.get("active"))
        decoder = tap.get("decoder")
        if isinstance(decoder, str) and decoder:
            enrich["videoLocalDecoderType"] = decoder
        fps = tap.get("fps")
        if isinstance(fps, (int, float)):
            enrich["videoLocalDecoderFps"] = float(fps)
    recording = _read_video_recording_state()
    if recording is not None:
        enrich["videoRecording"] = bool(recording)
    return enrich


def _get_local_ip() -> str:
    """Detect local IP via UDP socket probe."""
    try:
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        s.connect(("8.8.8.8", 80))
        ip = s.getsockname()[0]
        s.close()
        return ip
    except OSError:
        return "127.0.0.1"


async def main() -> None:
    config = load_config()
    configure_logging(config.logging.level)
    log = structlog.get_logger()
    log.info("cloud_service_starting")

    shutdown = asyncio.Event()
    loop = asyncio.get_event_loop()
    for sig_num in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig_num, shutdown.set)

    # Connect to state IPC to get telemetry
    state_client = StateIPCClient()
    try:
        await state_client.connect()
    except ConnectionError:
        log.warning("state_ipc_unavailable", msg="Running without telemetry")

    # Initialize pairing + MQTT
    from ados.core.pairing import PairingManager
    from ados.hal.detect import detect_board
    from ados.services.mavlink.state import VehicleState
    from ados.services.mqtt.gateway import MqttGateway

    pairing = PairingManager(state_path=config.pairing.state_path)
    convex_url = config.pairing.convex_url
    board = detect_board()
    start_time = time.monotonic()

    # CPU/memory history for sparklines
    cpu_history: deque[float] = deque(maxlen=60)
    memory_history: deque[float] = deque(maxlen=60)

    # VehicleState proxy updated from IPC
    vehicle_state = VehicleState()

    def _on_state_update(state_dict: dict) -> None:
        vehicle_state.update_from_dict(state_dict)
    state_client.set_state_handler(_on_state_update)

    mqtt = MqttGateway(config, vehicle_state, api_key=pairing.api_key)

    tasks = []

    # MQTT telemetry publishing
    tasks.append(asyncio.create_task(mqtt.run(shutdown), name="mqtt-gateway"))

    # State IPC reading with auto-retry
    async def state_reader_with_retry() -> None:
        """Read vehicle state from IPC, auto-reconnect on failure."""
        while not shutdown.is_set():
            try:
                if not state_client.connected:
                    await state_client.connect(retries=3, delay=2.0)
                await state_client.read_loop()
            except Exception as e:
                log.warning("state_ipc_read_failed", error=str(e))
            if not shutdown.is_set():
                log.info("state_ipc_reconnecting")
                await asyncio.sleep(2)

    if state_client.connected:
        tasks.append(asyncio.create_task(state_reader_with_retry(), name="state-reader"))

    # ── Pairing Beacon Loop (when NOT paired) ──────────────────

    async def pairing_beacon_loop() -> None:
        """When unpaired, POST pairing code to Convex every 30s for GCS discovery."""
        import httpx

        interval = getattr(config.pairing, "beacon_interval", 30)
        while not shutdown.is_set():
            if not pairing.is_paired and convex_url:
                try:
                    code = pairing.get_or_create_code()
                    api_key = pairing.generate_api_key()
                    local_ip = _get_local_ip()

                    beacon_body = {
                        "deviceId": config.agent.device_id,
                        "pairingCode": code,
                        "apiKey": api_key,
                        "name": getattr(config.agent, "name", "ADOS Agent"),
                        "version": __version__,
                        "board": board.name if board else "unknown",
                        "tier": board.tier if board else 0,
                        "mdnsHost": "",
                        "localIp": local_ip,
                    }
                    exp = pairing.code_expires_at()
                    if exp is not None:
                        beacon_body["pairingCodeExpiresAt"] = exp
                    async with httpx.AsyncClient(timeout=10.0) as client:
                        resp = await client.post(
                            f"{convex_url}/pairing/register",
                            json=beacon_body,
                        )
                        if resp.status_code == 200:
                            result = resp.json()
                            # If Convex says already claimed, detect pairing
                            if result.get("alreadyClaimed") or result.get("autoMatched"):
                                owner_id = result.get("userId", "cloud")
                                pairing.claim(owner_id, api_key)
                                log.info("pairing_claimed_via_beacon", owner=owner_id)
                    log.debug("pairing_beacon_sent", code=code)
                except Exception:
                    log.debug("pairing_beacon_failed")
            await asyncio.sleep(interval)

    tasks.append(asyncio.create_task(pairing_beacon_loop(), name="pairing-beacon"))

    # ── Cloud Heartbeat Loop (when paired) ─────────────────────

    async def heartbeat_loop() -> None:
        """When paired, POST full status to Convex every 5s."""
        import httpx

        # Cache service status to avoid running 10+ subprocess calls
        # every heartbeat. Refresh every 6th iteration (~30s).
        _cached_services: list[dict] = []
        _svc_refresh_counter = 0
        _SVC_REFRESH_INTERVAL = 6  # refresh every 6 heartbeats

        # Track the previous radio.paired flag so we can emit one
        # extra heartbeat 1s after the value changes. Without the
        # follow-up the GCS waits ~5s for the next regular tick to
        # see the new state, which reads as a UI flicker between
        # "pairing now" and "paired".
        _last_paired_flag: bool | None = None
        _emit_followup_at: float | None = None

        while not shutdown.is_set():
            # Re-check pairing state each iteration (may change via beacon)
            if pairing.is_paired and convex_url:
                try:
                    import psutil

                    vm = psutil.virtual_memory()
                    disk = psutil.disk_usage("/")
                    cpu_pct = psutil.cpu_percent(interval=0)
                    mem_pct = vm.percent
                    disk_pct = disk.percent
                    temp = None
                    temps = psutil.sensors_temperatures()
                    for key in ("cpu_thermal", "cpu-thermal", "coretemp"):
                        if key in temps and temps[key]:
                            temp = temps[key][0].current
                            break

                    cpu_history.append(cpu_pct)
                    memory_history.append(mem_pct)

                    # Refresh service status cache periodically (expensive:
                    # 10+ subprocess calls). Skip on most heartbeats to keep
                    # CPU free for the video encoder.
                    _svc_refresh_counter += 1
                    if _svc_refresh_counter >= _SVC_REFRESH_INTERVAL or not _cached_services:
                        _cached_services = await asyncio.to_thread(_get_services_status)
                        _svc_refresh_counter = 0

                    uptime = time.monotonic() - start_time

                    # Check if we received a heartbeat recently (within 10 seconds)
                    _last_hb = getattr(vehicle_state, "last_heartbeat", "")
                    _fc_connected = False
                    _fc_port = ""
                    _fc_baud = 0
                    if _last_hb:
                        try:
                            from datetime import datetime
                            hb_time = datetime.fromisoformat(_last_hb)
                            age = (datetime.now(hb_time.tzinfo) - hb_time).total_seconds()
                            _fc_connected = age < 10.0
                        except Exception:
                            _fc_connected = bool(_last_hb)

                    # Try to read FC port/baud from health file
                    try:
                        import json as _json
                        health_path = HEALTH_JSON
                        if health_path.exists():
                            health_data = _json.loads(health_path.read_text())
                            _fc_port = health_data.get("fc_port", "")
                            _fc_baud = health_data.get("fc_baud", 0)
                    except Exception:
                        pass

                    payload = {
                        "deviceId": config.agent.device_id,
                        "version": __version__,
                        "runtimeMode": "full",
                        "uptimeSeconds": round(uptime),
                        "boardName": board.name if board else "unknown",
                        "boardTier": board.tier if board else 0,
                        "boardSoc": board.soc if board else "",
                        "boardArch": board.arch if board else "",
                        "cpuPercent": cpu_pct,
                        "memoryPercent": mem_pct,
                        "diskPercent": disk_pct,
                        "temperature": temp if temp is not None else None,
                        "memoryUsedMb": round(vm.used / (1024 * 1024)),
                        "memoryTotalMb": round(vm.total / (1024 * 1024)),
                        "diskUsedGb": round(disk.used / (1024**3), 1),
                        "diskTotalGb": round(disk.total / (1024**3), 1),
                        "cpuCores": psutil.cpu_count() or 0,
                        "boardRamMb": round(vm.total / (1024 * 1024)),
                        "cpuHistory": list(cpu_history),
                        "memoryHistory": list(memory_history),
                        "fcConnected": _fc_connected,
                        "fcPort": _fc_port,
                        "fcBaud": _fc_baud,
                        "services": _cached_services,
                        "lastIp": _get_local_ip(),
                        "mdnsHost": "",
                        "setupUrl": (
                            f"http://{_get_local_ip()}:{config.scripting.rest_api.port}"
                        ),
                        "apiUrl": (
                            f"http://{_get_local_ip()}:{config.scripting.rest_api.port}/api"
                        ),
                        "agentVersion": __version__,
                    }

                    # Video pipeline status for GCS auto-discovery
                    _video_svc = next(
                        (s for s in payload["services"] if s["name"] == "ados-video"),
                        None,
                    )
                    payload["videoState"] = (
                        _video_svc["status"] if _video_svc else "stopped"
                    )
                    payload["videoWhepPort"] = (
                        8889 if _video_svc and _video_svc["status"] == "running" else 0
                    )
                    if payload["videoWhepPort"]:
                        payload["videoWhepUrl"] = (
                            f"http://{payload['lastIp']}:{payload['videoWhepPort']}/main/whep"
                        )

                    # MAVLink WebSocket proxy port for GCS direct connection
                    _mavlink_svc = next(
                        (s for s in payload["services"] if s["name"] == "ados-mavlink"),
                        None,
                    )
                    payload["mavlinkWsPort"] = (
                        8765 if _mavlink_svc and _mavlink_svc["status"] == "running" else 0
                    )
                    if payload["mavlinkWsPort"]:
                        payload["mavlinkWsUrl"] = (
                            f"ws://{payload['lastIp']}:{payload['mavlinkWsPort']}/"
                        )

                    remote = config.remote_access.cloudflare
                    if remote.setup_url:
                        payload["setupUrl"] = remote.setup_url
                    if remote.api_url:
                        payload["apiUrl"] = remote.api_url
                    if remote.video_whep_url:
                        payload["videoWhepUrl"] = remote.video_whep_url
                    if remote.mavlink_ws_url:
                        payload["mavlinkWsUrl"] = remote.mavlink_ws_url
                    # Mission Control URL is set by the operator when MC is
                    # reachable at a known address. Leave empty by default;
                    # the GCS uses its own URL when no advertised value
                    # exists. (config.server.cloud.url is the Convex relay,
                    # not Mission Control.)
                    mc_url = (
                        getattr(config.scripting, "mission_control_url", "") or ""
                    )
                    if mc_url:
                        payload["missionControlUrl"] = mc_url
                    payload["remoteAccess"] = {
                        "provider": config.remote_access.provider,
                        "publicUrls": config.remote_access.public_urls,
                    }

                    # Remove null temperature (Convex v.float64() rejects null)
                    if payload["temperature"] is None:
                        del payload["temperature"]

                    # Optional peripherals block. Currently carries the
                    # attached SPI LCD (if /etc/ados/display.conf is
                    # present). Mission Control's infer-capabilities
                    # filters peripherals[] for category="display" and
                    # populates the per-drone capability store.
                    _attached_display = _collect_attached_display()
                    if _attached_display is not None:
                        payload["peripherals"] = [_attached_display]

                    # Forward-compatible radio link block. The cloud
                    # subprocess does not own the WfbManager directly,
                    # so we ask the agent's REST surface on localhost
                    # with a tight budget. Any failure produces an
                    # `absent` block; the GCS keys off presence.
                    from ados.core.supervisor.heartbeat import (
                        build_radio_block,
                        fetch_wfb_status_via_http,
                    )
                    payload["radio"] = build_radio_block(
                        fetch_wfb_status_via_http()
                    )

                    # Pair-flag transition detection. When the radio
                    # crosses paired/unpaired we schedule a single
                    # follow-up heartbeat 1 s out so the GCS sees the
                    # new state without waiting on the next regular
                    # 5 s tick. Without this the UI lingers in the
                    # "pairing now" state long after the bind landed.
                    current_paired = bool(
                        payload.get("radio", {}).get("paired")
                    )
                    if (
                        _last_paired_flag is not None
                        and current_paired != _last_paired_flag
                    ):
                        _emit_followup_at = time.monotonic() + 1.0
                        log.info(
                            "pairing_transition_detected",
                            paired=current_paired,
                        )
                    _last_paired_flag = current_paired

                    # Surface the SPI LCD's active page so the GCS
                    # thumbnail can highlight which page is open. The
                    # OLED service writes ``/run/ados/lcd-state.json``
                    # on every navigator transition (including modal
                    # push/pop). Best-effort; the cloud subprocess
                    # does not own the navigator directly.
                    #
                    # Atomic-write semantics on the writer side mean a
                    # reader that catches the inode mid-rename can
                    # still see an empty file. Mirror the small retry
                    # loop the REST surface uses (see api/routes/
                    # display.py::_read_lcd_state) so a single race
                    # does not silently drop the field from the
                    # heartbeat.
                    if _attached_display is not None:
                        blob = _read_lcd_state_blob()
                        if isinstance(blob, dict):
                            modal_stack = blob.get("modal_stack")
                            if isinstance(modal_stack, list) and modal_stack:
                                payload["lcdActivePage"] = str(modal_stack[-1])
                            else:
                                active = blob.get("active_page_id")
                                if isinstance(active, str) and active:
                                    payload["lcdActivePage"] = active

                    # Display + decoder + theme enrichment for the
                    # GCS Display sub-view. Each field is optional —
                    # the helper returns only the keys it can fill in
                    # from the relevant local source (display.conf,
                    # touch.calib, lcd-video-tap.json, /api/video).
                    try:
                        enrich = _build_display_enrichment(
                            config,
                            has_attached_display=_attached_display is not None,
                            local_ip=payload.get("lastIp", _get_local_ip()),
                            api_port=config.scripting.rest_api.port,
                        )
                        payload.update(enrich)
                    except Exception as exc:
                        log.debug(
                            "heartbeat_display_enrichment_failed",
                            error=str(exc),
                        )

                    async with httpx.AsyncClient(timeout=10.0) as client:
                        resp = await client.post(
                            f"{convex_url}/agent/status",
                            json=payload,
                            headers={"X-ADOS-Key": pairing.api_key},
                        )
                        if resp.status_code == 200:
                            log.debug("cloud_status_sent")
                        else:
                            log.warning(
                                "cloud_status_rejected",
                                status=resp.status_code,
                                body=resp.text[:200],
                            )
                except Exception as exc:
                    log.debug("cloud_heartbeat_failed", error=str(exc))

            # Honour the pending follow-up tick when one was scheduled
            # by the transition detector above. Otherwise fall through
            # to the regular 5 s cadence.
            sleep_for = 5.0
            if _emit_followup_at is not None:
                remaining = _emit_followup_at - time.monotonic()
                if 0 < remaining < sleep_for:
                    sleep_for = remaining
                    _emit_followup_at = None  # consume
                elif remaining <= 0:
                    _emit_followup_at = None
            await asyncio.sleep(sleep_for)

    tasks.append(asyncio.create_task(heartbeat_loop(), name="heartbeat"))

    # ── Cloud Command Helpers ────────────────────────────────────

    def _get_recent_logs(limit: int = 200) -> list[dict]:
        """Read recent logs from journald."""
        import subprocess
        try:
            result = subprocess.run(
                ["journalctl", "-u", "ados-supervisor", "--no-pager", "-n", str(limit), "-o", "json"],
                capture_output=True, text=True, timeout=10,
            )
            if result.returncode != 0:
                return []
            entries = []
            for line in result.stdout.strip().splitlines():
                try:
                    import json as _json
                    entry = _json.loads(line)
                    entries.append({
                        "timestamp": entry.get("__REALTIME_TIMESTAMP", ""),
                        "level": entry.get("PRIORITY", "6"),
                        "message": entry.get("MESSAGE", ""),
                        "unit": entry.get("_SYSTEMD_UNIT", ""),
                    })
                except Exception:
                    continue
            return entries
        except Exception:
            return []

    def _list_scripts() -> list[dict]:
        """List script files in /var/ados/scripts/."""
        scripts_dir = SCRIPTS_DIR
        if not scripts_dir.exists():
            return []
        scripts = []
        for f in scripts_dir.glob("*.py"):
            scripts.append({
                "id": f.stem,
                "name": f.name,
                "path": str(f),
                "size": f.stat().st_size,
                "modified": f.stat().st_mtime,
            })
        return scripts

    def _list_suites() -> list[dict]:
        """List suite manifests in /etc/ados/suites/."""
        suites_dir = SUITES_DIR
        if not suites_dir.exists():
            return []
        suites = []
        for f in suites_dir.glob("*.yaml"):
            suites.append({
                "id": f.stem,
                "name": f.stem.replace("-", " ").title(),
                "path": str(f),
                "installed": True,
                "active": False,
            })
        return suites

    async def _execute_command(cmd: dict) -> tuple[str, dict | None, dict | None]:
        """Execute a cloud command and return (status, result, data).

        Heavy commands (get_services, get_logs, scan_peripherals) run in a
        thread via asyncio.to_thread() so they don't block the event loop.
        Blocking subprocess.run() calls in these functions were stalling the
        heartbeat task for 3-6s, causing false stale warnings in the GCS.
        """
        command = cmd.get("command", "")
        args = cmd.get("args") or {}

        try:
            if command in ("get_peripherals", "scan_peripherals"):
                from ados.api.routes.peripherals import _scan_all
                data = await asyncio.to_thread(_scan_all)
                return "completed", {"success": True, "message": "ok"}, data

            elif command == "get_services":
                data = await asyncio.to_thread(_get_services_status)
                return "completed", {"success": True, "message": "ok"}, data

            elif command == "get_logs":
                limit = args.get("limit", 200)
                data = await asyncio.to_thread(_get_recent_logs, limit)
                return "completed", {"success": True, "message": "ok"}, data

            elif command == "get_scripts":
                data = _list_scripts()
                return "completed", {"success": True, "message": "ok"}, data

            elif command == "get_suites":
                data = _list_suites()
                return "completed", {"success": True, "message": "ok"}, data

            elif command == "get_peers":
                return "completed", {"success": True, "message": "ok"}, []

            elif command == "get_enrollment":
                return "completed", {"success": True, "message": "ok"}, {"enrolled": False}

            elif command == "restart_service":
                name = args.get("name", "")
                # For now, just acknowledge - supervisor handles restarts
                return "completed", {"success": True, "message": f"Restart requested for {name}"}, None

            elif command == "wfb_pair_init_remote":
                # Cloud-relay path. The GS rig generates a fresh
                # libsodium keypair and ships the matching peer half
                # back via the command result. The GCS forwards that
                # blob to the drone via wfb_pair_apply_remote.
                #
                # Only valid on a GS rig. A drone rig responds with
                # `failed` so the orchestrator action surfaces the
                # error instead of silently corrupting state.
                import base64

                if config.agent.profile == "drone":
                    return "failed", {
                        "success": False,
                        "message": "wfb_pair_init_remote runs on the GS rig only",
                    }, None

                from ados.services.ground_station.pair_manager import (
                    apply_gs_keypair,
                )

                try:
                    # Generate the keypair into a tmpdir, persist the
                    # GS half locally as rx.key, return the drone half
                    # as a base64 blob for the GCS to relay.
                    import tempfile
                    from pathlib import Path

                    from ados.services.wfb.key_mgr import generate_key_pair

                    with tempfile.TemporaryDirectory() as tmp:
                        tx_path, rx_path = generate_key_pair(tmp)
                        # generate_key_pair renames to tx.key/rx.key.
                        # On the GS, the rx half stays here, the tx
                        # half (== drone.key bytes) goes to the peer.
                        drone_blob = Path(tx_path).read_bytes()
                        gs_blob = Path(rx_path).read_bytes()

                    peer_id = args.get("peerDeviceId") or args.get("peer_device_id")
                    pair_state = await apply_gs_keypair(gs_blob, peer_id)

                    return "completed", {"success": True, "message": "ok"}, {
                        "blobB64": base64.b64encode(drone_blob).decode("ascii"),
                        "fingerprint": pair_state.get("fingerprint"),
                        "gsDeviceId": config.agent.device_id,
                        "pairedAt": pair_state.get("paired_at"),
                    }
                except Exception as exc:  # noqa: BLE001
                    return "failed", {"success": False, "message": str(exc)}, None

            elif command == "wfb_pair_apply_remote":
                # Drone side. Receive the matching `drone.key` blob
                # produced by the GS's wfb_pair_init_remote and
                # persist it via PairManager. GS-only rigs reject.
                import base64

                if config.agent.profile != "drone":
                    return "failed", {
                        "success": False,
                        "message": "wfb_pair_apply_remote runs on the drone rig only",
                    }, None

                blob_b64 = args.get("blobB64") or args.get("blob_b64")
                peer_id = args.get("peerDeviceId") or args.get("peer_device_id")
                if not blob_b64:
                    return "failed", {
                        "success": False,
                        "message": "blobB64 required",
                    }, None

                try:
                    blob = base64.b64decode(blob_b64, validate=True)
                except (TypeError, ValueError) as exc:
                    return "failed", {
                        "success": False,
                        "message": f"blob_b64 decode failed: {exc}",
                    }, None

                try:
                    from ados.services.ground_station.pair_manager import (
                        apply_drone_keypair,
                    )

                    pair_state = await apply_drone_keypair(blob, peer_id)
                    return "completed", {"success": True, "message": "ok"}, {
                        "paired": True,
                        "fingerprint": pair_state.get("fingerprint"),
                        "pairedAt": pair_state.get("paired_at"),
                    }
                except Exception as exc:  # noqa: BLE001
                    return "failed", {"success": False, "message": str(exc)}, None

            elif command == "wfb_pair_unpair":
                # Either side. Wipe the local key and restart the
                # appropriate wfb unit. Used by the GCS's
                # `pairRigsRemote` action to roll back on fingerprint
                # mismatch and by an explicit operator unpair button.
                try:
                    from ados.services.ground_station.pair_manager import (
                        get_pair_manager,
                    )

                    role = "drone" if config.agent.profile == "drone" else "gs"
                    result = await get_pair_manager().unpair(role)
                    return "completed", {"success": True, "message": "ok"}, result
                except Exception as exc:  # noqa: BLE001
                    return "failed", {"success": False, "message": str(exc)}, None

            else:
                return "failed", {"success": False, "message": f"Unknown command: {command}"}, None

        except Exception as e:
            return "failed", {"success": False, "message": str(e)}, None

    # ── Cloud Command Polling (when paired) ────────────────────

    async def command_poll_loop() -> None:
        import httpx

        while not shutdown.is_set():
            if pairing.is_paired and convex_url:
                try:
                    async with httpx.AsyncClient(timeout=10.0) as client:
                        resp = await client.get(
                            f"{convex_url}/agent/commands",
                            params={"deviceId": config.agent.device_id},
                            headers={"X-ADOS-Key": pairing.api_key},
                        )
                        if resp.status_code == 200:
                            data = resp.json()
                            commands = data.get("commands", [])
                            for cmd in commands:
                                cmd_id = cmd.get("_id")
                                cmd_name = cmd.get("command", "unknown")
                                log.info("cloud_command_executing", command=cmd_name, id=cmd_id)

                                status, result, cmd_data = await _execute_command(cmd)

                                # ACK back to Convex
                                try:
                                    ack_payload: dict = {
                                        "commandId": cmd_id,
                                        "deviceId": config.agent.device_id,
                                        "status": status,
                                    }
                                    if result:
                                        ack_payload["result"] = result
                                    if cmd_data is not None:
                                        ack_payload["data"] = cmd_data

                                    ack_resp = await client.post(
                                        f"{convex_url}/agent/commands/ack",
                                        json=ack_payload,
                                        headers={"X-ADOS-Key": pairing.api_key},
                                    )
                                    if ack_resp.status_code == 200:
                                        log.info("cloud_command_acked", command=cmd_name, status=status)
                                    else:
                                        log.warning("cloud_command_ack_failed", command=cmd_name, http_status=ack_resp.status_code)
                                except Exception as ack_err:
                                    log.warning("cloud_command_ack_error", command=cmd_name, error=str(ack_err))
                except Exception:
                    log.debug("cloud_command_poll_failed")
            await asyncio.sleep(5)

    tasks.append(asyncio.create_task(command_poll_loop(), name="command-poll"))

    # ── WFB Auto-Pair Supervisor ──────────────────────
    # Runs the first-boot auto-bind loop here in ados-cloud
    # rather than inside ados-wfb / ados-wfb-rx because the bind
    # orchestrator stops + starts those wfb units to flip wfb-ng
    # profiles. Hosting the supervisor in the same service it's stopping
    # produces a self-kill loop. ados-cloud doesn't touch the radio so
    # it can systemctl-stop the wfb units without dying.

    try:
        from ados.services.wfb.auto_pair import get_auto_pair_supervisor

        ap_role = "drone" if config.agent.profile == "drone" else "gs"
        ap_supervisor = get_auto_pair_supervisor(ap_role)
        ap_supervisor.start()
        log.info("auto_pair_supervisor_spawned", role=ap_role)
    except Exception as exc:  # noqa: BLE001
        log.warning("auto_pair_supervisor_spawn_failed", error=str(exc))

    # ── MAVLink MQTT Relay (when paired) ──────────────────────

    async def mavlink_relay_task() -> None:
        """Relay raw MAVLink frames over MQTT for remote GCS access."""
        while not shutdown.is_set():
            if not pairing.is_paired:
                await asyncio.sleep(5)
                continue
            try:
                from ados.services.cloud.mavlink_relay import MavlinkMqttRelay

                relay = MavlinkMqttRelay(
                    device_id=config.agent.device_id,
                    broker=config.server.cloud.mqtt_broker,
                    port=config.server.cloud.mqtt_port,
                    transport=config.server.mqtt_transport,
                    username=f"ados-{config.agent.device_id}",
                    password=pairing.api_key or "",
                )
                await relay.start(shutdown)
            except Exception as exc:
                log.warning("mavlink_relay_failed", error=str(exc))
                await asyncio.sleep(5)

    tasks.append(asyncio.create_task(mavlink_relay_task(), name="mavlink-relay"))

    # ── WebRTC Signaling Relay (when paired) ──────────────────
    # Relays SDP offers/answers between MQTT and local mediamtx WHEP,
    # enabling P2P direct WebRTC across WAN. Browser dials in from
    # command.altnautica.com on any network; SDP handshake flows via
    # MQTT, media flows direct peer-to-peer after ICE punching.

    async def webrtc_signaling_task() -> None:
        """Relay WebRTC SDP offers/answers over MQTT for cross-network video."""
        while not shutdown.is_set():
            if not pairing.is_paired:
                await asyncio.sleep(5)
                continue
            try:
                from ados.services.cloud.webrtc_signaling import WebrtcSignalingRelay

                relay = WebrtcSignalingRelay(
                    device_id=config.agent.device_id,
                    broker=config.server.cloud.mqtt_broker,
                    port=config.server.cloud.mqtt_port,
                    transport=config.server.mqtt_transport,
                    username=f"ados-{config.agent.device_id}",
                    password=pairing.api_key or "",
                )
                await relay.start(shutdown)
            except Exception as exc:
                log.warning("webrtc_signaling_failed", error=str(exc))
                await asyncio.sleep(5)

    tasks.append(asyncio.create_task(webrtc_signaling_task(), name="webrtc-signaling"))

    log.info("cloud_service_ready", paired=pairing.is_paired)
    await shutdown.wait()

    log.info("cloud_service_stopping")
    for task in tasks:
        task.cancel()
    await asyncio.gather(*tasks, return_exceptions=True)
    await state_client.disconnect()
    log.info("cloud_service_stopped")


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
    sys.exit(0)
