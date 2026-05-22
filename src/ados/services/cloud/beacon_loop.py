"""Pairing-beacon loop. POSTs the unpaired pairing code to Convex.

Runs only while the agent is unpaired and only when
``config.pairing.beacon_enabled`` is True. The default deployment ships
with the beacon OFF — the agent stays LAN-only until an operator hits
the local POST /api/pairing/claim. Operators who want cloud-discovery
flip the flag and the beacon publishes the code every
``beacon_interval`` seconds.
"""

from __future__ import annotations

import asyncio

import httpx

from ados import __version__

from ._context import CloudContext
from .heartbeat import get_local_ip as _get_local_ip


async def pairing_beacon_loop(ctx: CloudContext) -> None:
    """When unpaired, POST pairing code to Convex every ``beacon_interval`` s."""
    config = ctx.config
    pairing = ctx.pairing
    convex_url = ctx.convex_url
    shutdown = ctx.shutdown
    log = ctx.log
    board = ctx.board

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
                # Stable across beacon iterations so the apiKey on
                # cmd_pairingRequests stays consistent and survives
                # the claim → pairing.json transition without drift.
                api_key = pairing.get_or_create_api_key()
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
                # Code value intentionally omitted — log entries flow
                # into the SSE log stream which is reachable from any
                # browser on the same origin pre-pair. Logging the
                # active code there would broadcast it to anyone who
                # opens the dashboard during a pairing window.
                log.debug("pairing_beacon_sent")
            except Exception:
                log.debug("pairing_beacon_failed")
        await asyncio.sleep(interval)


__all__ = ["pairing_beacon_loop"]
