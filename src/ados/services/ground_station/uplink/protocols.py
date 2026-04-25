"""Structural protocol every uplink manager satisfies, plus an inert stub.

The router only uses `is_up`, `get_iface`, and `get_gateway`. The modem
manager also exposes `data_usage` for the cap tracker.

`_StubManager` lets `UplinkRouter` run before the real Ethernet or WiFi
client managers are wired in. `is_up()` returns False so the stubbed
uplink never passes `_viable_uplinks()`. The real manager replaces the
stub through the router constructor.
"""

from __future__ import annotations

from typing import Optional, Protocol

__all__ = [
    "UplinkManagerProto",
    "_StubManager",
]


class UplinkManagerProto(Protocol):
    """Structural type every uplink manager satisfies."""

    async def is_up(self) -> bool: ...
    def get_iface(self) -> str: ...
    async def get_gateway(self) -> Optional[str]: ...


class _StubManager:
    """Inert placeholder for unwired uplink slots."""

    def __init__(self, iface: str) -> None:
        self._iface = iface

    async def is_up(self) -> bool:
        return False

    def get_iface(self) -> str:
        return self._iface

    async def get_gateway(self) -> Optional[str]:
        return None
