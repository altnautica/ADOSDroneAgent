"""Captive portal — iptables rules + portal check URL handlers.

Ported from ADOS Agent Lite. Redirects HTTP/HTTPS traffic on the AP
interface to the agent's REST API, triggering OS captive portal detection
on Android, iOS, and Windows devices.
"""

from __future__ import annotations

import asyncio
import sys

from ados.core.logging import get_logger

log = get_logger("services.network.captive_portal")


class CaptivePortal:
    """Manages iptables DNAT rules for captive portal redirect."""

    def __init__(self, ap_ip: str = "192.168.4.1", api_port: int = 8080, enabled: bool = True):
        self.ap_ip = ap_ip
        self.api_port = api_port
        self.enabled = enabled
        self._rules_applied = False

    async def setup(self, iface: str = "wlan0") -> None:
        """Apply iptables rules to redirect HTTP traffic to agent API."""
        if sys.platform != "linux" or not self.enabled:
            return

        rules = [
            ["iptables", "-t", "nat", "-A", "PREROUTING", "-i", iface,
             "-p", "tcp", "--dport", "80", "-j", "REDIRECT", "--to-port", str(self.api_port)],
            ["iptables", "-t", "nat", "-A", "PREROUTING", "-i", iface,
             "-p", "tcp", "--dport", "443", "-j", "REDIRECT", "--to-port", str(self.api_port)],
        ]

        for cmd in rules:
            proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.DEVNULL,
            )
            await proc.wait()

        self._rules_applied = True
        log.info("captive_portal_enabled", iface=iface)

    async def cleanup(self, iface: str = "wlan0") -> None:
        """Remove iptables rules."""
        if not self._rules_applied:
            return

        rules = [
            ["iptables", "-t", "nat", "-D", "PREROUTING", "-i", iface,
             "-p", "tcp", "--dport", "80", "-j", "REDIRECT", "--to-port", str(self.api_port)],
            ["iptables", "-t", "nat", "-D", "PREROUTING", "-i", iface,
             "-p", "tcp", "--dport", "443", "-j", "REDIRECT", "--to-port", str(self.api_port)],
        ]

        for cmd in rules:
            proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.DEVNULL,
            )
            await proc.wait()

        self._rules_applied = False
        log.info("captive_portal_disabled")


# ------------------------------------------------------------------
# Route handler helpers for captive portal detection URLs.
# These return redirect URLs for Android/Apple/Windows captive checks.
# The caller (FastAPI/Starlette router) should mount these at:
#   /generate_204       (Android)
#   /hotspot-detect.html (Apple)
#   /connecttest.txt     (Windows)
# ------------------------------------------------------------------

def get_captive_redirect_url(ap_ip: str, api_port: int) -> str:
    """Return the URL to redirect captive portal checks to."""
    return f"http://{ap_ip}:{api_port}/"
