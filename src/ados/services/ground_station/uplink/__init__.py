"""Uplink router package.

Re-exports the public surface that callers historically imported from
`ados.services.ground_station.uplink_router`. The legacy module is
preserved as a thin re-export shim so existing imports keep working.
"""

from __future__ import annotations

from .adapters import _EthernetAdapter, _ModemAdapter, _WifiClientAdapter
from .data_cap import DataCapTracker, _UsageState
from .events import DataCapState, UplinkEvent, UplinkEventBus, UplinkEventKind
from .protocols import UplinkManagerProto, _StubManager
from .router import UplinkRouter, get_uplink_router, main

__all__ = [
    "UplinkEvent",
    "UplinkEventBus",
    "UplinkEventKind",
    "DataCapState",
    "UplinkManagerProto",
    "_StubManager",
    "_ModemAdapter",
    "_WifiClientAdapter",
    "_EthernetAdapter",
    "_UsageState",
    "DataCapTracker",
    "UplinkRouter",
    "get_uplink_router",
    "main",
]
