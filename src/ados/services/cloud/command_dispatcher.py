"""Cloud-command dispatcher.

Inbound commands arrive via ``GET /agent/commands`` and are dispatched
by name. Each handler returns ``(status, result, data)`` where status
is ``"completed"`` or ``"failed"``, ``result`` is the small ACK dict
the cloud relay surfaces in the UI, and ``data`` is the optional
larger payload (logs, scripts, peripherals, plugin status).

Plugin lifecycle commands route through ``RemoteInstallReceiver`` so
the idempotency contract is identical across the local-LAN install path
and this cloud-relay fallback path.
"""

from __future__ import annotations

import asyncio

from ados.core.paths import SCRIPTS_DIR, SUITES_DIR

from ._context import CloudContext
from .heartbeat import get_services_status as _get_services_status


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


async def execute_command(  # noqa: C901
    ctx: CloudContext, cmd: dict,
) -> tuple[str, dict | None, dict | None]:
    """Execute a cloud command and return (status, result, data).

    Heavy commands (get_services, get_logs, scan_peripherals) run in a
    thread via asyncio.to_thread() so they don't block the event loop.
    Blocking subprocess.run() calls in these functions were stalling the
    heartbeat task for 3-6s, causing false stale warnings in the GCS.
    """
    config = ctx.config
    pairing = ctx.pairing
    convex_url = ctx.convex_url

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

        elif command == "plugin.install":
            # Cloud-relay install fallback. The local-first path is
            # the multipart upload to /api/plugins/install which the
            # GCS uses whenever it has a direct LAN line. This
            # branch fires only when the GCS could not reach us.
            from ados.api.routes.plugins import _get_supervisor
            from ados.plugins.remote_install import RemoteInstallReceiver

            return await RemoteInstallReceiver.handle_install(
                cmd,
                supervisor=_get_supervisor(),
                device_id=config.agent.device_id,
                api_key=pairing.api_key,
                convex_url=convex_url,
            )

        elif command in (
            "plugin.uninstall",
            "plugin.enable",
            "plugin.disable",
            "plugin.configure",
        ):
            # Non-install lifecycle commands route through the same
            # receiver so idempotency + ack shape stay consistent
            # with the install path.
            from ados.api.routes.plugins import _get_supervisor
            from ados.plugins.remote_install import RemoteInstallReceiver

            return await RemoteInstallReceiver.dispatch(
                cmd,
                supervisor=_get_supervisor(),
                device_id=config.agent.device_id,
            )

        else:
            return "failed", {"success": False, "message": f"Unknown command: {command}"}, None

    except Exception as e:
        return "failed", {"success": False, "message": str(e)}, None


__all__ = [
    "execute_command",
    "_get_recent_logs",
    "_list_scripts",
    "_list_suites",
]
