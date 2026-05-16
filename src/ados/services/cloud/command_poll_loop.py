"""Cloud command polling loop.

Polls ``GET /agent/commands`` every 5 s, dispatches each command via
``command_dispatcher.execute_command``, and ACKs the result back to
Convex. Authentication is always via the ``X-ADOS-Key`` request
header — never in a URL or query string.
"""

from __future__ import annotations

import asyncio

import httpx

from ._context import CloudContext
from .command_dispatcher import execute_command


async def command_poll_loop(ctx: CloudContext) -> None:
    """When paired, poll Convex for pending commands and execute them."""
    config = ctx.config
    pairing = ctx.pairing
    convex_url = ctx.convex_url
    shutdown = ctx.shutdown
    log = ctx.log

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

                            status, result, cmd_data = await execute_command(ctx, cmd)

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


__all__ = ["command_poll_loop"]
