"""Cloud reachability probing for the uplink router.

A successful TCP connect to port 443 of the cloud relay is treated as a
strong proxy for full reachability of the Cloudflare-fronted Convex
endpoint. We do not run a full TLS handshake from stdlib so the
dependency footprint stays minimal.

The probe can be bound to a specific interface via SO_BINDTODEVICE so
that the test exercises the path the router currently selected, not
whatever the kernel default route happens to be at probe time.
"""

from __future__ import annotations

import asyncio
import socket
from typing import Optional

import structlog

__all__ = [
    "HEALTH_INTERVAL_SECONDS",
    "HEALTH_TIMEOUT_SECONDS",
    "HEALTH_HOST",
    "HEALTH_PORT",
    "HEALTH_PATH",
    "probe_host",
]

log = structlog.get_logger(__name__)

HEALTH_INTERVAL_SECONDS = 15.0
HEALTH_TIMEOUT_SECONDS = 5.0
HEALTH_HOST = "convex.altnautica.com"
HEALTH_PORT = 443
HEALTH_PATH = "/"


async def probe_host(iface: Optional[str]) -> bool:
    """TCP connect to the cloud relay, optionally bound to an iface.

    Returns True on a successful connect, False on DNS failure, timeout,
    or any socket error. SO_BINDTODEVICE requires CAP_NET_RAW. When the
    capability is missing we fall back to a plain connect on the current
    default route, which still validates reachability.
    """
    loop = asyncio.get_running_loop()
    try:
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.setblocking(False)
        if iface is not None:
            try:
                sock.setsockopt(
                    socket.SOL_SOCKET,
                    socket.SO_BINDTODEVICE,
                    iface.encode("ascii"),
                )
            except (PermissionError, OSError) as exc:
                log.debug(
                    "uplink.bind_iface_failed",
                    iface=iface,
                    error=str(exc),
                )
        try:
            addr_info = await loop.getaddrinfo(
                HEALTH_HOST, HEALTH_PORT, type=socket.SOCK_STREAM
            )
        except OSError as exc:
            log.debug("uplink.dns_failed", error=str(exc))
            sock.close()
            return False
        if not addr_info:
            sock.close()
            return False
        family, socktype, proto, _, sockaddr = addr_info[0]
        if family != socket.AF_INET:
            sock.close()
            sock = socket.socket(family, socktype, proto)
            sock.setblocking(False)
        try:
            await asyncio.wait_for(
                loop.sock_connect(sock, sockaddr),
                timeout=HEALTH_TIMEOUT_SECONDS,
            )
            return True
        except (asyncio.TimeoutError, OSError) as exc:
            log.debug("uplink.connect_failed", iface=iface, error=str(exc))
            return False
        finally:
            try:
                sock.close()
            except OSError:
                pass
    except Exception as exc:
        log.debug("uplink.probe_exc", error=str(exc))
        return False
