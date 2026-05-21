"""Cloud heartbeat loop. POSTs the full status payload to Convex.

Composes the payload from psutil + the vehicle state + cached service
status + the display-enrichment helpers in ``heartbeat.py``. Cadence
is 5 s with a one-shot 1 s follow-up tick whenever the radio
``paired`` flag transitions so the GCS does not linger in the
"pairing now" state across a 5 s window.

Authentication is always via the ``X-ADOS-Key`` request header. The
API key is never sent in a URL or query string — that policy is
asserted by the cloud command auth audit test.
"""

from __future__ import annotations

import asyncio
import time
from typing import TYPE_CHECKING

import httpx

from ados import __version__
from ados.core.paths import HEALTH_JSON

from ._context import CloudContext
from .heartbeat import (
    build_can_buses_enrichment as _build_can_buses_enrichment,
)
from .heartbeat import (
    build_display_enrichment as _build_display_enrichment,
)
from .heartbeat import (
    build_display_type_enrichment as _build_display_type_enrichment,
)
from .heartbeat import (
    collect_attached_display as _collect_attached_display,
)
from .heartbeat import (
    get_local_ip as _get_local_ip,
)
from .heartbeat import (
    get_services_status as _get_services_status,
)
from .heartbeat import (
    read_lcd_state_blob as _read_lcd_state_blob,
)

if TYPE_CHECKING:
    pass


async def heartbeat_loop(ctx: CloudContext) -> None:  # noqa: C901
    """When paired, POST full status to Convex every 5 s."""
    config = ctx.config
    pairing = ctx.pairing
    convex_url = ctx.convex_url
    shutdown = ctx.shutdown
    log = ctx.log
    board = ctx.board
    start_time = ctx.start_time
    vehicle_state = ctx.vehicle_state
    cpu_history = ctx.cpu_history
    memory_history = ctx.memory_history

    # Cache service status to avoid running 10+ subprocess calls
    # every heartbeat. Refresh every 6th iteration (~30s).
    _cached_services: list[dict] = []
    _svc_refresh_counter = 0
    _svc_refresh_interval = 6  # refresh every 6 heartbeats

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
                if _svc_refresh_counter >= _svc_refresh_interval or not _cached_services:
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

                from ados.core.profile import current_profile_and_role
                _profile, _role = current_profile_and_role(config)

                payload = {
                    "deviceId": config.agent.device_id,
                    "version": __version__,
                    "runtimeMode": "full",
                    "profile": _profile,
                    "role": _role,
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

                # Plugin auto-update freshness for the GCS drone-detail
                # panel. Maximum of every install's last-check timestamp;
                # absent when nothing has been polled yet. Best-effort —
                # heartbeat must not fail on auxiliary data.
                try:
                    from ados.plugins.auto_update import (
                        latest_check_timestamp_ms as _latest_plugin_check,
                    )

                    _last_plugin_check = _latest_plugin_check()
                    if _last_plugin_check is not None:
                        payload["last_plugin_update_check_at"] = _last_plugin_check
                except Exception as exc:
                    log.debug(
                        "heartbeat_plugin_update_check_failed",
                        error=str(exc),
                    )

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
                    fetch_wfb_status_via_http(api_key=pairing.api_key)
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
                        api_key=pairing.api_key,
                    )
                    payload.update(enrich)
                except Exception as exc:
                    log.debug(
                        "heartbeat_display_enrichment_failed",
                        error=str(exc),
                    )

                # FC CAN bus configuration. Reads the persisted
                # parameter cache and emits a ``canBuses`` array so
                # the GCS can surface the per-port driver / bitrate /
                # protocol on the drone card. The helper omits the
                # field entirely when no CAN params are loaded yet,
                # which is the warmup window between FC connect and
                # the parameter download finishing. Failure-tolerant:
                # the heartbeat must not break on cache-read errors.
                try:
                    payload.update(_build_can_buses_enrichment())
                except Exception as exc:
                    log.debug(
                        "heartbeat_can_buses_enrichment_failed",
                        error=str(exc),
                    )

                # Effective local-display primary path. Folded in next
                # to the LCD-rooted fields above so the GCS can show a
                # single "displayType" pill (HDMI / LCD / none) without
                # having to re-derive it from the other enrichment
                # fields. Failure-tolerant for the same reason as the
                # block above.
                try:
                    payload.update(_build_display_type_enrichment(config))
                except Exception as exc:
                    log.debug(
                        "heartbeat_display_type_enrichment_failed",
                        error=str(exc),
                    )

                # Convex's v.optional(T) accepts "field absent OR T",
                # not explicit null. Strip any top-level None-valued
                # fields so a drone-profile heartbeat (role=None) or
                # any other future optional field doesn't 500 the
                # mutation on validation. Nested objects pass through
                # — their internal validators handle their own
                # null-vs-absent semantics.
                payload = {k: v for k, v in payload.items() if v is not None}

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


__all__ = ["heartbeat_loop"]
