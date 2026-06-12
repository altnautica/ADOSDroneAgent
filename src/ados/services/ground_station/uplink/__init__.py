"""Uplink router package.

Exposes the public surface (the `UplinkRouter` FSM, the `get_uplink_router`
factory the REST routes call in-process, the data-cap tracker, and the event
bus). The native `ados-net` daemon is the only runtime that drives the router;
there is no standalone Python entrypoint.
"""

from __future__ import annotations

from .adapters import _EthernetAdapter, _ModemAdapter, _WifiClientAdapter
from .data_cap import DataCapTracker, _UsageState
from .events import DataCapState, UplinkEvent, UplinkEventBus, UplinkEventKind
from .protocols import UplinkManagerProto, _StubManager
from .router import UplinkRouter
from .service import get_uplink_router

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
]
