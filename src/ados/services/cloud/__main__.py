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

import structlog

from ados import __version__
from ados.core.config import load_config
from ados.core.ipc import StateIPCClient
from ados.core.logging import configure_logging


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
                    proc = psutil.Process(pid)
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

                    async with httpx.AsyncClient(timeout=10.0) as client:
                        resp = await client.post(
                            f"{convex_url}/pairing/register",
                            json={
                                "deviceId": config.agent.device_id,
                                "pairingCode": code,
                                "apiKey": api_key,
                                "name": getattr(config.agent, "name", "ADOS Agent"),
                                "version": __version__,
                                "board": board.name if board else "unknown",
                                "tier": board.tier if board else 0,
                                "mdnsHost": "",
                                "localIp": local_ip,
                            },
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
                        from pathlib import Path
                        health_path = Path("/run/ados/health.json")
                        if health_path.exists():
                            health_data = _json.loads(health_path.read_text())
                            _fc_port = health_data.get("fc_port", "")
                            _fc_baud = health_data.get("fc_baud", 0)
                    except Exception:
                        pass

                    payload = {
                        "deviceId": config.agent.device_id,
                        "apiKey": pairing.api_key,
                        "version": __version__,
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
                        "services": _get_services_status(),
                        "lastIp": _get_local_ip(),
                        "mdnsHost": "",
                        "agentVersion": __version__,
                    }

                    # Remove null temperature (Convex v.float64() rejects null)
                    if payload["temperature"] is None:
                        del payload["temperature"]

                    async with httpx.AsyncClient(timeout=10.0) as client:
                        resp = await client.post(f"{convex_url}/agent/status", json=payload)
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
            await asyncio.sleep(5)

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
        from pathlib import Path
        scripts_dir = Path("/var/ados/scripts")
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
        from pathlib import Path
        suites_dir = Path("/etc/ados/suites")
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
        """Execute a cloud command and return (status, result, data)."""
        command = cmd.get("command", "")
        args = cmd.get("args") or {}

        try:
            if command in ("get_peripherals", "scan_peripherals"):
                from ados.api.routes.peripherals import _scan_all
                data = _scan_all()
                return "completed", {"success": True, "message": "ok"}, data

            elif command == "get_services":
                data = _get_services_status()
                return "completed", {"success": True, "message": "ok"}, data

            elif command == "get_logs":
                limit = args.get("limit", 200)
                data = _get_recent_logs(limit)
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
                            params={
                                "deviceId": config.agent.device_id,
                                "apiKey": pairing.api_key,
                            },
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
                                        "apiKey": pairing.api_key,
                                        "status": status,
                                    }
                                    if result:
                                        ack_payload["result"] = result
                                    if cmd_data is not None:
                                        ack_payload["data"] = cmd_data

                                    ack_resp = await client.post(
                                        f"{convex_url}/agent/commands/ack",
                                        json=ack_payload,
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
