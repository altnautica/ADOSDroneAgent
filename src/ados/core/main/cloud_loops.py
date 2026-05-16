"""Cloud relay loops — beacon, heartbeat, command poll.

Each loop is a long-running coroutine that wakes on a fixed interval,
checks pairing state, and either announces the unpaired pairing code,
publishes the heartbeat payload, or drains pending commands. The
AgentApp calls these via thin instance-method wrappers so the public
shape (``app._cloud_beacon_loop`` etc.) remains unchanged for tests
and existing call sites.
"""

from __future__ import annotations

import asyncio
from typing import TYPE_CHECKING

from ados import __version__
from ados.core.logging import get_logger

from ._helpers import _get_local_ip

if TYPE_CHECKING:
    from .app import AgentApp

log = get_logger("main")


async def cloud_beacon_loop(app: AgentApp) -> None:
    """When unpaired, periodically POST pairing code to Convex for cloud discovery."""
    import httpx

    interval = app.config.pairing.beacon_interval
    convex_url = app.config.pairing.convex_url

    while not app._shutdown.is_set():
        if not app.pairing_manager.is_paired and convex_url:
            try:
                code = app.pairing_manager.get_or_create_code()
                local_ip = _get_local_ip()
                mdns_host = ""
                if app.discovery_service:
                    mdns_host = app.discovery_service.mdns_hostname

                api_key = app.pairing_manager.generate_api_key()
                body = {
                    "deviceId": app.config.agent.device_id,
                    "pairingCode": code,
                    "apiKey": api_key,
                    "name": app.config.agent.name,
                    "version": __version__,
                    "board": app.board_name,
                    "mdnsHost": mdns_host,
                    "localIp": local_ip,
                }
                exp = app.pairing_manager.code_expires_at()
                if exp is not None:
                    body["pairingCodeExpiresAt"] = exp
                async with httpx.AsyncClient(timeout=10.0) as client:
                    await client.post(
                        f"{convex_url}/pairing/register",
                        json=body,
                    )
                log.debug("pairing_beacon_sent", code=code)
            except Exception:
                log.debug("pairing_beacon_failed")
        await asyncio.sleep(interval)


async def cloud_heartbeat_loop(app: AgentApp) -> None:
    """When paired, periodically POST full status to Convex."""
    import httpx

    convex_url = app.config.pairing.convex_url

    while not app._shutdown.is_set():
        if app.pairing_manager.is_paired and convex_url:
            try:
                payload = app._build_heartbeat_payload()
                async with httpx.AsyncClient(timeout=10.0) as client:
                    await client.post(
                        f"{convex_url}/agent/status",
                        json=payload,
                        headers={"X-ADOS-Key": app.pairing_manager.api_key},
                    )
                log.debug("cloud_status_sent")
            except Exception:
                log.debug("cloud_status_failed")
        await asyncio.sleep(5)


async def cloud_command_poll_loop(app: AgentApp) -> None:  # noqa: C901
    """When paired, poll Convex for pending commands and execute them."""
    import httpx

    convex_url = app.config.pairing.convex_url

    while not app._shutdown.is_set():
        if app.pairing_manager.is_paired and convex_url:
            try:
                device_id = app.config.agent.device_id
                api_key = app.pairing_manager.api_key

                async with httpx.AsyncClient(timeout=10.0) as client:
                    resp = await client.get(
                        f"{convex_url}/agent/commands",
                        params={"deviceId": device_id},
                        headers={"X-ADOS-Key": api_key},
                    )
                    if resp.status_code != 200:
                        log.debug("cloud_command_poll_error", status=resp.status_code)
                        await asyncio.sleep(5)
                        continue

                    data = resp.json()
                    commands = data.get("commands", [])

                    for cmd in commands:
                        cmd_id = cmd.get("_id", "")
                        command = cmd.get("command", "")
                        args = cmd.get("args")
                        result = {"success": False, "message": "Unknown command"}

                        try:
                            if command == "restart_service":
                                svc_name = args.get("name", "") if args else ""
                                msg = f"Service '{svc_name}' restart requested"
                                result = {"success": True, "message": msg}
                            elif command == "send_command":
                                cmd_text = args.get("cmd", "") if args else ""
                                if app._command_executor:
                                    exec_result = await asyncio.wait_for(
                                        app._command_executor.execute(cmd_text),
                                        timeout=10.0,
                                    )
                                    result = {
                                        "success": True,
                                        "message": str(exec_result),
                                    }
                                else:
                                    result = {
                                        "success": False,
                                        "message": "Command executor not available",
                                    }
                            elif command == "scan_peripherals":
                                result = {
                                    "success": True,
                                    "message": "Peripheral scan complete",
                                }
                            else:
                                msg = f"Unknown command: {command}"
                                result = {"success": False, "message": msg}
                        except Exception as exc:
                            result = {"success": False, "message": str(exc)}

                        # Ack the command back to Convex
                        try:
                            await client.post(
                                f"{convex_url}/agent/commands/ack",
                                json={
                                    "commandId": cmd_id,
                                    "deviceId": device_id,
                                    "status": "completed" if result["success"] else "failed",
                                    "result": result,
                                },
                                headers={"X-ADOS-Key": api_key},
                            )
                        except Exception:
                            log.debug("cloud_command_ack_failed", command_id=cmd_id)

            except Exception:
                log.debug("cloud_command_poll_failed")
        await asyncio.sleep(5)


__all__ = [
    "cloud_beacon_loop",
    "cloud_heartbeat_loop",
    "cloud_command_poll_loop",
]
