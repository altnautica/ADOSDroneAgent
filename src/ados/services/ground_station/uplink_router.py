"""Re-export shim for the decomposed uplink package.

The implementation now lives under `ados.services.ground_station.uplink`.
This module preserves the historical import path so downstream callers
(`cloud_relay_bridge`, `share_uplink_firewall`, REST routes, the test
suite) keep working without edits.
"""

from __future__ import annotations

from .uplink import (
    DataCapState,
    DataCapTracker,
    UplinkEvent,
    UplinkEventBus,
    UplinkEventKind,
    UplinkManagerProto,
    UplinkRouter,
    _EthernetAdapter,
    _ModemAdapter,
    _StubManager,
    _UsageState,
    _WifiClientAdapter,
    get_uplink_router,
    main,
)

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


if __name__ == "__main__":
    main()
