"""mDNS advertisement for the MCP server.

Publishes _ados-mcp._tcp on every agent-owned interface so MCP clients
on the local network can discover the drone without hardcoding an IP.

TXT records:
  version   agent version string
  device_id short 8-char device ID
  port      MCP HTTP port (default 8090)

Uses the zeroconf library which is already a core agent dependency.
"""

from __future__ import annotations

import socket

import structlog
from zeroconf import Zeroconf, ServiceInfo

log = structlog.get_logger()

_SERVICE_TYPE = "_ados-mcp._tcp.local."


class McpMdns:
    """Registers and unregisters the MCP mDNS service."""

    def __init__(
        self,
        port: int,
        device_id: str,
        agent_version: str,
    ) -> None:
        self._port = port
        self._device_id = device_id
        self._agent_version = agent_version
        self._zeroconf: Zeroconf | None = None
        self._info: ServiceInfo | None = None

    def start(self) -> None:
        try:
            hostname = socket.gethostname()
            local_ip = _get_local_ip()

            self._zeroconf = Zeroconf()
            self._info = ServiceInfo(
                type_=_SERVICE_TYPE,
                name=f"ADOS MCP {self._device_id}.{_SERVICE_TYPE}",
                addresses=[socket.inet_aton(local_ip)],
                port=self._port,
                properties={
                    "version": self._agent_version,
                    "device_id": self._device_id,
                    "port": str(self._port),
                    "hostname": hostname,
                },
                server=f"{hostname}.local.",
            )
            self._zeroconf.register_service(self._info)
            log.info(
                "mcp_mdns_registered",
                device_id=self._device_id,
                address=local_ip,
                port=self._port,
            )
        except Exception as e:
            log.warning("mcp_mdns_registration_failed", error=str(e))

    def stop(self) -> None:
        if self._zeroconf and self._info:
            try:
                self._zeroconf.unregister_service(self._info)
                self._zeroconf.close()
            except Exception:
                pass
        self._zeroconf = None
        self._info = None
        log.info("mcp_mdns_unregistered")


def _get_local_ip() -> str:
    """Return the primary non-loopback IP address."""
    try:
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        s.connect(("8.8.8.8", 80))
        ip = s.getsockname()[0]
        s.close()
        return ip
    except OSError:
        return "127.0.0.1"
