"""Cloud-heartbeat payload composer.

Extracted from the AgentApp so the dict-shape construction is testable
in isolation and the rest of the supervisor stays focused on lifecycle
concerns. Pulled out as a free function taking the AgentApp because
most of the input data lives on the app instance.
"""

from __future__ import annotations

import json
import platform
import subprocess
import time
from typing import TYPE_CHECKING

from ados import __version__
from ados.core.paths import INSTALL_RESULT, WFB_MODULE_SOURCE
from ados.core.service_tracker import ServiceState

from ._helpers import _get_local_ip

if TYPE_CHECKING:
    from .app import AgentApp

# WFB kernel module name probed via ``modinfo -n`` to locate the loaded
# module file. The directory the path lands in tells us how it was
# installed: ``updates/`` for a prebuilt package, ``extra/`` (or a path
# containing ``dkms``) for a DKMS build.
_WFB_MODULE_NAME = "8812eu"
_MODINFO_TIMEOUT_S = 3.0


def _read_install_result() -> dict | None:
    """Best-effort read of the install-result record.

    Returns the parsed dict, or ``None`` if the file is absent,
    unreadable, or not valid JSON. Never raises — a missing or garbage
    install record must not break the heartbeat.
    """
    try:
        data = json.loads(INSTALL_RESULT.read_text())
    except (OSError, ValueError, json.JSONDecodeError):
        return None
    return data if isinstance(data, dict) else None


def _wfb_module_source_from_modinfo() -> str | None:
    """Authoritative radio-module source from the loaded module's path.

    Runs ``modinfo -n <module>`` and classifies the returned path. The
    Wi-Fi driver is always built on the device via DKMS now, which installs
    under ``/lib/modules/*/updates/dkms/`` or ``/lib/modules/*/extra/``, so
    any such on-disk path ⇒ ``"dkms"``.

    Returns ``None`` when modinfo is absent, the module is not loaded,
    the command times out, or the path does not match the pattern.
    Best-effort; never raises.
    """
    try:
        result = subprocess.run(
            ["modinfo", "-n", _WFB_MODULE_NAME],
            capture_output=True,
            text=True,
            timeout=_MODINFO_TIMEOUT_S,
            check=False,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if result.returncode != 0:
        return None
    path = (result.stdout or "").strip()
    if not path:
        return None
    if "/updates/" in path or "/extra/" in path or "dkms" in path:
        return "dkms"
    return None


def _resolve_wfb_module_source() -> str:
    """Resolve the radio-module source across reboot.

    Resolution order: live modinfo path (source of truth) → the tmpfs
    breadcrumb (fast hint, vanishes on reboot) → the install-result
    record → ``"none"``. Best-effort; never raises.
    """
    from_modinfo = _wfb_module_source_from_modinfo()
    if from_modinfo:
        return from_modinfo

    try:
        crumb = WFB_MODULE_SOURCE.read_text().strip()
    except OSError:
        crumb = ""
    if crumb in ("prebuilt", "dkms"):
        return crumb

    install = _read_install_result()
    if install is not None:
        src = install.get("wfbModuleSource")
        if src in ("prebuilt", "dkms"):
            return src

    return "none"


def build_heartbeat_payload(app: AgentApp) -> dict:  # noqa: C901
    """Build the cloud heartbeat payload dict.

    Extracted from ``AgentApp._cloud_heartbeat_loop`` for direct
    testability. Reads only from the running app instance.
    """
    local_ip = _get_local_ip()
    mdns_host = ""
    if app.discovery_service:
        mdns_host = app.discovery_service.mdns_hostname

    fc_connected = False
    fc_port = ""
    fc_baud = 0
    if app._fc_connection:
        fc_connected = getattr(app._fc_connection, "connected", False)
        fc_port = getattr(app.config.mavlink, "port", "")
        fc_baud = getattr(app.config.mavlink, "baud", 0)

    # Board info from detection
    board = getattr(app, "_board", None)
    board_tier = board.tier if board else 0
    board_soc = board.soc if board else ""
    board_arch = board.arch if board else ""

    # Health info from monitor
    health = app.health
    cpu_percent = getattr(health, "cpu_percent", 0.0)
    memory_percent = getattr(health, "memory_percent", 0.0)
    disk_percent = getattr(health, "disk_percent", 0.0)
    temperature = getattr(health, "temperature", None)

    # Service states with accurate operational status
    service_list: list[dict] = []
    all_services = app.services.get_all()

    # Infer true state from runtime conditions
    def _svc_state(name: str, state: ServiceState) -> str:
        s = state.value
        if s != "running":
            return s
        if name == "fc-connection":
            fc = getattr(app, "_fc_connection", None)
            if fc and not getattr(fc, "connected", False):
                return "degraded"
        elif name == "video-pipeline":
            if getattr(app.config.video, "mode", "disabled") == "disabled":
                return "stopped"
        elif name == "wfb-link":
            wfb = getattr(app, "_wfb_manager", None)
            if wfb and not getattr(wfb, "has_adapter", False):
                return "degraded"
        elif name == "pairing-beacon":
            if app.pairing_manager.is_paired:
                return "stopped"
        return s

    # Get process-level metrics (single process, all services share)
    proc_cpu = 0.0
    proc_rss_mb = 0.0
    mem_used_mb = 0
    mem_total_mb = 0
    disk_used_gb = 0.0
    disk_total_gb = 0.0
    cpu_cores = 0
    try:
        import os as _os

        import psutil as _psutil
        _proc = _psutil.Process(_os.getpid())
        proc_cpu = _proc.cpu_percent(interval=0)
        proc_rss_mb = _proc.memory_info().rss / (1024 * 1024)
        _vm = _psutil.virtual_memory()
        mem_used_mb = round(_vm.used / (1024 * 1024))
        mem_total_mb = round(_vm.total / (1024 * 1024))
        _disk = _psutil.disk_usage("/")
        disk_used_gb = round(_disk.used / (1024**3), 1)
        disk_total_gb = round(_disk.total / (1024**3), 1)
        cpu_cores = _psutil.cpu_count() or 0
    except Exception:
        pass

    # Track CPU/memory history for sparkline charts (5s interval, 5 min window)
    app._cpu_history.append(cpu_percent)
    app._memory_history.append(memory_percent)

    # Video pipeline restart counter, exposed for the GCS health
    # view. Defensive against an absent or older pipeline.
    vp = getattr(app, "_video_pipeline", None)
    try:
        video_restart_attempts = (
            int(vp.restart_attempts()) if vp is not None else 0
        )
    except Exception:
        video_restart_attempts = 0

    # Per-service data with real uptime (no fake CPU/RAM distribution)
    now_mono = time.monotonic()
    for svc_name, svc_state in all_services.items():
        real_state = _svc_state(svc_name, svc_state)
        # Compute per-service uptime from transition history
        svc_uptime = 0.0
        transitions = app.services.get_transitions(svc_name)
        if transitions:
            for ts, st in reversed(transitions):
                if st == ServiceState.RUNNING:
                    svc_uptime = now_mono - ts
                    break
        service_list.append({
            "name": svc_name,
            "status": real_state,
            "uptimeSeconds": round(svc_uptime),
        })

    # LAN-routable URL block. The GCS surfaces these as "manual
    # connection" fallbacks when the cloud-relay flow is degraded
    # or the operator wants a direct LAN connection from a desktop
    # GCS. Built from local_ip so they survive a hostname rename
    # and from the live config so any MAVLink endpoint flip
    # propagates on the next tick.
    mav_tcp_port = app._first_mavlink_tcp_port_for_heartbeat()
    mav_ws_port = app._first_mavlink_ws_port_for_heartbeat()
    video_whep_port: int | None = None
    vp = getattr(app, "_video_pipeline", None)
    if vp is not None:
        try:
            vp_status = vp.get_status()
            video_whep_port = int(
                vp_status.get("mediamtx", {}).get("webrtc_port", 8889)
            )
        except Exception:
            video_whep_port = None
    if video_whep_port is None:
        # Ground-station profile runs `ados-mediamtx-gs` independently
        # of `app._video_pipeline` (which is the drone-side pipeline
        # and stays None on a ground-station node). Probe the public
        # WHEP endpoint directly so the heartbeat carries the LAN
        # video URL whenever MediaMTX is serving frames, regardless
        # of which service spawned it.
        try:
            from ados.api.routes.video import (
                _MEDIAMTX_WEBRTC_PORT,
                mediamtx_whep_alive_sync,
            )

            if mediamtx_whep_alive_sync():
                video_whep_port = _MEDIAMTX_WEBRTC_PORT
        except Exception:
            video_whep_port = None
    manual_connection_urls: dict[str, str | None] = {
        "mavlinkTcp": (
            f"tcp://{local_ip}:{mav_tcp_port}" if mav_tcp_port and local_ip else None
        ),
        "mavlinkWs": (
            f"ws://{local_ip}:{mav_ws_port}/" if mav_ws_port and local_ip else None
        ),
        "videoViewer": (
            f"http://{local_ip}:{video_whep_port}/main/"
            if video_whep_port and local_ip
            else None
        ),
    }

    # Cloud relay = MQTT-to-Convex pair. Cloudflare = inbound tunnel.
    # These are distinct concepts. The GCS used to conflate them
    # under a single "Remote" label; surface them separately here
    # so the heartbeat consumer can render an accurate state.
    cloud_relay_url: str | None = None
    if app.pairing_manager.is_paired:
        server = getattr(app.config, "server", None)
        mode = getattr(server, "mode", "") if server else ""
        if mode == "self_hosted":
            sh = getattr(server, "self_hosted", None)
            cloud_relay_url = str(getattr(sh, "url", "") or "") or None
        elif mode == "cloud":
            cloud = getattr(server, "cloud", None)
            cloud_relay_url = str(getattr(cloud, "url", "") or "") or None
    cloudflare_url: str | None = None
    cf_remote = app.config.remote_access.cloudflare
    if getattr(cf_remote, "enabled", False):
        cloudflare_url = (
            str(getattr(cf_remote, "setup_url", "") or "") or None
        )

    payload: dict = {
        "deviceId": app.config.agent.device_id,
        "version": __version__,
        "uptimeSeconds": app.uptime_seconds,
        "boardName": app.board_name,
        "boardTier": board_tier,
        "boardSoc": board_soc,
        "boardArch": board_arch,
        "cpuPercent": cpu_percent,
        "memoryPercent": memory_percent,
        "diskPercent": disk_percent,
        "temperature": temperature,
        # Absolute resource values
        "memoryUsedMb": mem_used_mb,
        "memoryTotalMb": mem_total_mb,
        "diskUsedGb": disk_used_gb,
        "diskTotalGb": disk_total_gb,
        "cpuCores": cpu_cores,
        "boardRamMb": mem_total_mb,
        # Process-level totals (single-process architecture)
        "processCpuPercent": round(proc_cpu, 1),
        "processMemoryMb": round(proc_rss_mb, 1),
        # History arrays for sparkline charts
        "cpuHistory": list(app._cpu_history),
        "memoryHistory": list(app._memory_history),
        "fcConnected": fc_connected,
        "fcPort": fc_port,
        "fcBaud": fc_baud,
        "services": service_list,
        "lastIp": local_ip,
        "mdnsHost": mdns_host,
        "setupUrl": f"http://{local_ip}:8080",
        "apiUrl": f"http://{local_ip}:8080/api",
        "agentVersion": __version__,
        "videoRestartAttempts": video_restart_attempts,
        "manualConnectionUrls": manual_connection_urls,
        "cloudRelayUrl": cloud_relay_url,
        "cloudflareUrl": cloudflare_url,
    }

    # Install health + kernel/radio-module telemetry. The GCS surfaces
    # these so an operator can spot a degraded or failed install and the
    # provenance of the loaded WFB radio module without SSH'ing in.
    # All reads are best-effort: a missing or garbage install record
    # leaves the defaults in place and never breaks the heartbeat.
    payload["kernelRelease"] = platform.release()
    payload["wfbModuleSource"] = _resolve_wfb_module_source()
    _install = _read_install_result()
    if _install is not None:
        status = _install.get("status")
        payload["installStatus"] = (
            status if isinstance(status, str) and status else "unknown"
        )
        version = _install.get("version")
        if isinstance(version, str) and version:
            payload["installVersion"] = version
        failed = _install.get("failedSteps")
        payload["failedSteps"] = failed if isinstance(failed, list) else []
    else:
        payload["installStatus"] = "unknown"
        payload["failedSteps"] = []

    # Foxglove bind status. Defaults to False when no ROS manager is
    # attached; flips True only after a started ROS container fails the
    # localhost TCP probe on the configured foxglove port.
    rm = getattr(app, "_ros_manager", None)
    try:
        payload["foxgloveBindFailed"] = (
            bool(rm.foxglove_bind_failed()) if rm is not None else False
        )
    except Exception:
        payload["foxgloveBindFailed"] = False

    remote = app.config.remote_access.cloudflare
    if remote.setup_url:
        payload["setupUrl"] = remote.setup_url
    if remote.api_url:
        payload["apiUrl"] = remote.api_url
    if remote.video_whep_url:
        # Operator-configured Cloudflare tunnel WHEP URL. Distinct from
        # the auto-derived LAN path that was retired with the
        # manualConnectionUrls.videoWhep field — operators that point
        # this at a working WHEP endpoint still get a usable URL.
        payload["videoWhepUrl"] = remote.video_whep_url
    if remote.mavlink_ws_url:
        payload["mavlinkWsUrl"] = remote.mavlink_ws_url
    # Surface the prior URL on the tick the value changes so the GCS can
    # drain stale connections without waiting for a new dial. Only the
    # config-driven URL is tracked here; live rotation observation is
    # future work.
    current_mavlink_ws_url = payload.get("mavlinkWsUrl")
    if current_mavlink_ws_url is not None:
        if (
            app._last_mavlink_ws_url is not None
            and app._last_mavlink_ws_url != current_mavlink_ws_url
        ):
            payload["mavlinkWsUrlPrev"] = app._last_mavlink_ws_url
        app._last_mavlink_ws_url = current_mavlink_ws_url
    payload["missionControlUrl"] = app.config.server.cloud.url
    # Cloud posture chosen by the operator (or the install-time default).
    # Mission Control reads this to distinguish "intentionally local" from
    # "offline" when a paired drone stops emitting heartbeats.
    payload["cloudPosture"] = str(getattr(app.config.server, "mode", "local") or "local")
    payload["remoteAccess"] = {
        "provider": app.config.remote_access.provider,
        "publicUrls": app.config.remote_access.public_urls,
    }

    # Forward-compatible radio link block — sourced from
    # the in-process WfbManager directly when present.
    from ados.core.supervisor.heartbeat import build_radio_block
    wfb = getattr(app, "_wfb_manager", None)
    wfb_status: dict | None = None
    if wfb is not None:
        try:
            wfb_status = wfb.get_status()
        except Exception:
            wfb_status = None
    payload["radio"] = build_radio_block(wfb_status)

    # Top-level radio-adapter selection verdict. Mirrors the same two
    # fields inside the radio block but hoists them to the payload root
    # so Mission Control and the CLI can read the injection check
    # without unpacking the radio sub-object. chipset is null and
    # injectionOk is false when no RTL injection-capable adapter was
    # found/verified — the stranded radio link signal.
    if isinstance(wfb_status, dict):
        payload["wfbAdapterChipset"] = wfb_status.get("adapter_chipset")
        payload["wfbAdapterInjectionOk"] = bool(
            wfb_status.get("adapter_injection_ok", False)
        )
    else:
        payload["wfbAdapterChipset"] = None
        payload["wfbAdapterInjectionOk"] = False

    # Inter-rig peer presence — sourced cross-process from
    # /run/ados/peer-presence.json, written by the HopListener every
    # time a WFB-radio PresenceBeacon decodes successfully. Fields
    # stay None until the radio link delivers a beacon and freshness
    # is within the staleness window.
    import json as _json

    from ados.core.paths import PEER_PRESENCE_JSON
    _PEER_STALE_AFTER_S = 60.0
    try:
        peer = _json.loads(PEER_PRESENCE_JSON.read_text())
    except (OSError, ValueError):
        peer = None
    if isinstance(peer, dict):
        last_seen = peer.get("peer_last_seen_unix")
        fresh = (
            isinstance(last_seen, (int, float))
            and (time.time() - float(last_seen)) <= _PEER_STALE_AFTER_S
        )
        if fresh:
            payload["peerDeviceId"] = peer.get("peer_device_id")
            payload["peerRole"] = peer.get("peer_role")
            payload["peerChannel"] = peer.get("peer_channel")
            payload["peerRssiDbm"] = peer.get("peer_rssi_dbm")
            payload["peerSeenAtUnix"] = last_seen

    # Camera state — sourced cross-process from
    # /run/ados/camera-state.json, written by the ados-video pipeline
    # on every discover_and_assign() pass. Surfaces "missing" on the
    # GCS drone card so operators can spot a missing or wedged camera
    # without SSH'ing in. Stale snapshots (> 5 min since last write)
    # are treated as unknown rather than authoritative.
    from ados.core.paths import CAMERA_STATE_JSON
    _CAMERA_STALE_AFTER_S = 300.0
    try:
        camera = _json.loads(CAMERA_STATE_JSON.read_text())
    except (OSError, ValueError):
        camera = None
    if isinstance(camera, dict):
        updated_at = camera.get("updated_at_unix")
        cam_fresh = (
            isinstance(updated_at, (int, float))
            and (time.time() - float(updated_at)) <= _CAMERA_STALE_AFTER_S
        )
        if cam_fresh:
            state = camera.get("state")
            if isinstance(state, str) and state in ("ready", "missing", "error"):
                payload["cameraState"] = state

    # Plugin inventory — webapp-side installs are not visible to the GCS
    # otherwise. Best-effort: a missing or partially-initialised
    # supervisor leaves the field absent rather than failing the tick.
    # The supervisor singleton is per-process; the heartbeat runs in
    # the agent main process while plugin installs happen in the API
    # process, so we re-load state from disk on every tick to keep the
    # inventory fresh (the on-disk plugin-state.json file is the single
    # source of truth, ~10s of plugins max — load cost is negligible).
    try:
        from ados.plugins.state import load_state as _load_plugin_state

        installs = _load_plugin_state()
        payload["pluginInventory"] = [
            {
                "plugin_id": inst.plugin_id,
                "version": getattr(inst, "version", None),
                "status": getattr(inst, "status", None),
            }
            for inst in installs
        ]
    except Exception:
        pass

    # Peripheral connection states. Drives the per-peripheral
    # connected/disconnected dot + "last seen Ns ago" line on the GCS
    # drone card. Cached at the registry level (5 s TTL) so this is a
    # cheap dict copy on the heartbeat hot path.
    try:
        from ados.services.peripherals.registry import get_peripheral_registry

        payload["peripheralStates"] = get_peripheral_registry().states()
    except Exception:
        pass
    return payload


__all__ = ["build_heartbeat_payload"]
