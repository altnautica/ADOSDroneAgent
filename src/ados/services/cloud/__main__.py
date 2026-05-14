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
    HEALTH_JSON,
    SCRIPTS_DIR,
    SUITES_DIR,
)
from ados.services.cloud.heartbeat import (
    _proc_cache,  # noqa: F401  (re-exported for tests/observability)
    _read_json_with_retry,
    build_display_enrichment as _build_display_enrichment,
    collect_attached_display as _collect_attached_display,
    get_local_ip as _get_local_ip,
    get_services_status as _get_services_status,
    read_display_rotation as _read_display_rotation,
    read_lcd_state_blob as _read_lcd_state_blob,
    read_lcd_video_tap as _read_lcd_video_tap,
    read_recent_touch as _read_recent_touch,
    read_touch_calib_status as _read_touch_calib_status,
    read_video_recording_state as _read_video_recording_state,
)

# Re-export the path constants the helper module uses so existing
# tests that ``patch.object(cloud_main, "DISPLAY_CONF_PATH", ...)``
# keep working unchanged. The helpers in ``heartbeat`` look up these
# names from their own module, so callers patching here will also
# need to patch the heartbeat module if they actually want to redirect
# the underlying file read. See ``tests/test_heartbeat_enrichment.py``.
from ados.core.paths import (  # noqa: E402  re-export
    DISPLAY_CONF_PATH,
    LCD_VIDEO_TAP_PATH,
    TOUCH_CALIB_PATH,
)


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
        # Cloud pair beacon is opt-in. When disabled the agent stays
        # LAN-only and waits for a direct POST /api/pairing/claim.
        beacon_enabled = getattr(config.pairing, "beacon_enabled", False)
        if not beacon_enabled:
            log.info(
                "pairing_beacon_disabled",
                reason="config.pairing.beacon_enabled is False",
            )
            return
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
        # Pass the service shutdown event so a SIGTERM during a long-
        # running rendezvous tears down the in-flight bind cleanly. The
        # orchestrator's cancel hook kills any leftover socat for us.
        ap_supervisor = get_auto_pair_supervisor(ap_role, shutdown_event=shutdown)
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
