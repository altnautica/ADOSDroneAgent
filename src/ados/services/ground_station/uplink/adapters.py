"""Adapters that wrap concrete managers in the uplink protocol shape.

The concrete managers (`GroundStationModemManager`, `WifiClientManager`,
`EthernetManager`) expose `status()` and other methods that do not match
the router's `is_up / get_iface / get_gateway` interface directly. These
adapters bridge the gap without modifying the manager modules.

`_ModemAdapter` also forwards `data_usage()` so `DataCapTracker` keeps
working against the same handle.
"""

from __future__ import annotations

import asyncio
from typing import Any, Optional

import structlog

__all__ = [
    "_ModemAdapter",
    "_WifiClientAdapter",
    "_EthernetAdapter",
]

log = structlog.get_logger(__name__)


class _ModemAdapter:
    """Adapts `GroundStationModemManager` to the uplink protocol."""

    def __init__(self, modem: Any, iface: str = "wwan0") -> None:
        self._modem = modem
        self._iface = iface

    async def is_up(self) -> bool:
        try:
            st = await self._modem.status()
        except Exception as exc:
            log.debug("uplink.modem_status_failed", error=str(exc))
            return False
        state = str(st.get("state", "")).lower()
        if state in ("connected", "registered", "online"):
            return True
        return bool(st.get("ip")) or bool(st.get("connected"))

    def get_iface(self) -> str:
        getter = getattr(self._modem, "_current_iface", None)
        if callable(getter):
            try:
                return getter()
            except Exception:
                pass
        return self._iface

    async def get_gateway(self) -> Optional[str]:
        iface = self.get_iface()
        try:
            proc = await asyncio.create_subprocess_exec(
                "ip", "-4", "route", "show", "default", "dev", iface,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.DEVNULL,
            )
            stdout, _ = await proc.communicate()
            text = stdout.decode(errors="replace")
            for line in text.splitlines():
                parts = line.split()
                if "default" in parts and "via" in parts:
                    i = parts.index("via")
                    if i + 1 < len(parts):
                        return parts[i + 1]
        except (OSError, asyncio.CancelledError):
            return None
        return None

    async def data_usage(self) -> dict:
        return await self._modem.data_usage()


class _WifiClientAdapter:
    """Adapts `WifiClientManager` to the uplink protocol via status()."""

    def __init__(self, wifi: Any, iface: str = "wlan0_client") -> None:
        self._wifi = wifi
        self._iface = iface

    async def is_up(self) -> bool:
        try:
            st = await self._wifi.status()
        except Exception as exc:
            log.debug("uplink.wifi_status_failed", error=str(exc))
            return False
        return bool(st.get("connected")) and bool(st.get("ip"))

    def get_iface(self) -> str:
        inner = getattr(self._wifi, "_interface", None)
        if isinstance(inner, str) and inner:
            return inner
        return self._iface

    async def get_gateway(self) -> Optional[str]:
        try:
            st = await self._wifi.status()
        except Exception:
            return None
        gw = st.get("gateway")
        return gw if isinstance(gw, str) and gw else None


class _EthernetAdapter:
    """Adapts `EthernetManager` to the uplink protocol via status()."""

    def __init__(self, eth: Any, iface: str = "eth0") -> None:
        self._eth = eth
        self._iface = iface

    async def is_up(self) -> bool:
        try:
            st = await self._eth.status()
        except Exception as exc:
            log.debug("uplink.eth_status_failed", error=str(exc))
            return False
        return bool(st.get("link")) and bool(st.get("ip"))

    def get_iface(self) -> str:
        inner = getattr(self._eth, "_interface", None)
        if isinstance(inner, str) and inner:
            return inner
        return self._iface

    async def get_gateway(self) -> Optional[str]:
        try:
            st = await self._eth.status()
        except Exception:
            return None
        gw = st.get("gateway")
        return gw if isinstance(gw, str) and gw else None
