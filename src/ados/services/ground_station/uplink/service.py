"""Uplink router construction: the `get_uplink_router` factory the REST routes
call in-process to build the singleton router with concrete managers.

The native ``ados-net`` daemon is the only runtime that drives the router; there
is no standalone Python service entrypoint.
"""

from __future__ import annotations

import structlog

from .router import UplinkRouter

__all__ = [
    "get_uplink_router",
]

log = structlog.get_logger(__name__)

_instance: UplinkRouter | None = None


def get_uplink_router() -> UplinkRouter:
    global _instance
    if _instance is None:
        _instance = _build_router_with_concrete_managers()
    return _instance


def _build_router_with_concrete_managers() -> UplinkRouter:
    """Construct the router wrapping the real singleton managers.

    Imports are local so test harnesses or callers that want a stub-only
    router can still instantiate `UplinkRouter()` directly without
    pulling in NetworkManager or ModemManager dependencies.
    """
    from .adapters import _EthernetAdapter, _ModemAdapter, _WifiClientAdapter

    try:
        from ados.services.ground_station.ethernet_manager import (
            get_ethernet_manager,
        )
        from ados.services.ground_station.modem_manager import (
            get_modem_manager,
        )
        from ados.services.ground_station.wifi_client_manager import (
            get_wifi_client_manager,
        )
    except Exception as exc:
        log.warning("uplink.manager_import_failed", error=str(exc))
        return UplinkRouter()

    try:
        modem_raw = get_modem_manager()
        wifi_raw = get_wifi_client_manager()
        eth_raw = get_ethernet_manager()
    except Exception as exc:
        log.warning("uplink.manager_init_failed", error=str(exc))
        return UplinkRouter()

    return UplinkRouter(
        modem_manager=_ModemAdapter(modem_raw),
        wifi_client_manager=_WifiClientAdapter(wifi_raw),
        ethernet_manager=_EthernetAdapter(eth_raw),
    )


