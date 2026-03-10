"""mDNS discovery service using zeroconf."""

from __future__ import annotations

import asyncio
import socket
from typing import Optional

from ados.core.logging import get_logger

log = get_logger("discovery")

SERVICE_TYPE = "_ados._tcp.local."


class DiscoveryService:
    """Registers and manages mDNS service for local network discovery."""

    def __init__(
        self,
        device_id: str,
        port: int = 8080,
        name: str = "my-drone",
        version: str = "0.2.0",
        board: str = "unknown",
    ):
        self._device_id = device_id
        self._port = port
        self._name = name
        self._version = version
        self._board = board
        self._zeroconf = None
        self._info = None
        self._short_id = device_id[:6].lower()

    @property
    def mdns_hostname(self) -> str:
        return f"ados-{self._short_id}.local"

    def _get_local_ip(self) -> str:
        """Get the primary local IP address."""
        try:
            s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            s.connect(("8.8.8.8", 80))
            ip = s.getsockname()[0]
            s.close()
            return ip
        except OSError:
            return "127.0.0.1"

    def _build_txt_records(
        self,
        paired: bool = False,
        code: Optional[str] = None,
        owner: Optional[str] = None,
    ) -> dict:
        records = {
            "device_id": self._device_id,
            "version": self._version,
            "board": self._board,
            "name": self._name,
            "paired": str(paired).lower(),
        }
        if code and not paired:
            records["code"] = code
        if owner and paired:
            records["owner"] = owner
        return records

    async def register(
        self,
        paired: bool = False,
        code: Optional[str] = None,
        owner: Optional[str] = None,
    ) -> None:
        """Register mDNS service on the local network."""
        try:
            from zeroconf import IPVersion
            from zeroconf.asyncio import AsyncServiceInfo, AsyncZeroconf

            local_ip = self._get_local_ip()
            txt_records = self._build_txt_records(paired, code, owner)
            service_name = f"ADOS-{self._short_id}.{SERVICE_TYPE}"

            self._info = AsyncServiceInfo(
                SERVICE_TYPE,
                service_name,
                addresses=[socket.inet_aton(local_ip)],
                port=self._port,
                properties=txt_records,
                server=f"ados-{self._short_id}.local.",
            )

            self._zeroconf = AsyncZeroconf(ip_version=IPVersion.V4Only)
            await self._zeroconf.async_register_service(self._info)
            log.info(
                "discovery_registered",
                service=service_name,
                ip=local_ip,
                port=self._port,
                hostname=self.mdns_hostname,
            )
        except Exception as e:
            log.warning("discovery_register_failed", error=str(e))
            # mDNS is optional, don't crash the agent
            self._zeroconf = None
            self._info = None

    async def update_txt(
        self,
        paired: bool = False,
        code: Optional[str] = None,
        owner: Optional[str] = None,
    ) -> None:
        """Update TXT records (e.g., after pairing state changes)."""
        if not self._zeroconf or not self._info:
            return
        try:
            from zeroconf.asyncio import AsyncServiceInfo

            txt_records = self._build_txt_records(paired, code, owner)
            local_ip = self._get_local_ip()
            service_name = self._info.name

            new_info = AsyncServiceInfo(
                SERVICE_TYPE,
                service_name,
                addresses=[socket.inet_aton(local_ip)],
                port=self._port,
                properties=txt_records,
                server=f"ados-{self._short_id}.local.",
            )
            await self._zeroconf.async_update_service(new_info)
            self._info = new_info
            log.debug("discovery_txt_updated", paired=paired)
        except Exception as e:
            log.warning("discovery_update_failed", error=str(e))

    async def unregister(self) -> None:
        """Unregister mDNS service."""
        if self._zeroconf:
            try:
                if self._info:
                    await self._zeroconf.async_unregister_service(self._info)
                await self._zeroconf.async_close()
                log.info("discovery_unregistered")
            except Exception as e:
                log.warning("discovery_unregister_failed", error=str(e))
            finally:
                self._zeroconf = None
                self._info = None
