"""Systemd service entry, manager wiring, and data-cap throttle bridge.

This module owns the long-running process side of the uplink router:
constructing the singleton with concrete managers, kicking the WiFi /
Ethernet poll loops, subscribing to the router event bus to apply data
cap throttling, and the asyncio main loop with signal handling.
"""

from __future__ import annotations

import asyncio
import signal
from typing import Optional

import structlog

from .router import UplinkRouter

__all__ = [
    "get_uplink_router",
    "main",
]

log = structlog.get_logger(__name__)

_instance: "UplinkRouter | None" = None


def get_uplink_router() -> "UplinkRouter":
    global _instance
    if _instance is None:
        _instance = _build_router_with_concrete_managers()
    return _instance


def _build_router_with_concrete_managers() -> "UplinkRouter":
    """Construct the router wrapping the real singleton managers.

    Imports are local so test harnesses or callers that want a stub-only
    router can still instantiate `UplinkRouter()` directly without
    pulling in NetworkManager or ModemManager dependencies.
    """
    from .adapters import _EthernetAdapter, _ModemAdapter, _WifiClientAdapter

    try:
        from ados.services.ground_station.modem_manager import (
            get_modem_manager,
        )
        from ados.services.ground_station.wifi_client_manager import (
            get_wifi_client_manager,
        )
        from ados.services.ground_station.ethernet_manager import (
            get_ethernet_manager,
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


def _start_manager_polling() -> None:
    """Kick the periodic poll loops on the WiFi and Ethernet managers.

    The modem manager has no standalone polling loop. WiFi exposes
    `start_polling()` with a 10s cadence. Ethernet exposes
    `start_polling()` with a 5s cadence. Both are idempotent on a
    running event loop.
    """
    try:
        from ados.services.ground_station.wifi_client_manager import (
            get_wifi_client_manager,
        )
        from ados.services.ground_station.ethernet_manager import (
            get_ethernet_manager,
        )
    except Exception as exc:
        log.debug("uplink.poll_import_failed", error=str(exc))
        return

    try:
        get_wifi_client_manager().start_polling()
    except Exception as exc:
        log.warning("uplink.wifi_polling_start_failed", error=str(exc))

    try:
        get_ethernet_manager().start_polling()
    except Exception as exc:
        log.warning("uplink.eth_polling_start_failed", error=str(exc))


async def _run_data_cap_throttle_consumer(router: "UplinkRouter") -> None:
    """Subscribe to the router bus and apply throttle on cap transitions.

    Severity ladder:
      warn_80     -> INFO  (remove any throttle, restore NAT)
      throttle_95 -> WARN  (install 256 kbit tbf qdisc on active iface)
      blocked_100 -> ERROR (drop MASQUERADE rule, hard block)

    Direct-call wiring: the consumer resolves the active uplink's
    interface at event-time by asking the router, and calls
    `share_uplink_firewall.apply_throttle`. Errors are best-effort
    logged and never crash the loop.
    """
    try:
        from ados.services.ground_station.share_uplink_firewall import (
            apply_throttle as _apply_throttle,
        )
    except Exception as exc:
        log.warning("uplink.throttle_import_failed", error=str(exc))
        return

    try:
        async for evt in router.bus.subscribe():
            if evt.kind != "data_cap_threshold":
                continue
            state = evt.data_cap_state
            if state is None:
                continue

            active_iface: Optional[str] = None
            active_name = router.active_uplink
            if active_name:
                try:
                    active_iface = await router._uplink_iface(active_name)  # noqa: SLF001
                except Exception as exc:
                    log.debug(
                        "uplink.throttle_iface_lookup_failed",
                        error=str(exc),
                    )

            if state == "ok":
                log.debug(
                    "uplink.datacap_throttle_applied",
                    state=state,
                    iface=active_iface,
                )
            elif state == "warn_80":
                log.info(
                    "uplink.datacap_warn_80",
                    iface=active_iface,
                    note="usage crossed 80 percent of cellular cap",
                )
            elif state == "throttle_95":
                log.warning(
                    "uplink.datacap_throttle_95",
                    iface=active_iface,
                    rate_kbps=256,
                )
            elif state == "blocked_100":
                log.error(
                    "uplink.datacap_blocked_100",
                    iface=active_iface,
                    note="cellular cap reached; NAT forwarding dropped",
                )

            try:
                result = await _apply_throttle(active_iface, state)
                log.info("uplink.datacap_throttle_result", result=result)
            except Exception as exc:
                log.warning(
                    "uplink.datacap_throttle_apply_failed",
                    state=state,
                    error=str(exc),
                )
    except asyncio.CancelledError:
        return
    except Exception as exc:
        log.warning("uplink.datacap_throttle_consumer_exc", error=str(exc))


async def _run_service() -> None:
    router = get_uplink_router()
    _start_manager_polling()
    router.bind_manager_events()
    stop_event = asyncio.Event()

    loop = asyncio.get_running_loop()

    def _handle_signal(signame: str) -> None:
        log.info("uplink.signal_received", signal=signame)
        stop_event.set()

    for signame in ("SIGINT", "SIGTERM"):
        try:
            loop.add_signal_handler(
                getattr(signal, signame), _handle_signal, signame
            )
        except NotImplementedError:
            pass

    await router.start()
    log.info(
        "uplink.service_ready",
        priority=router.get_priority(),
        active=router.active_uplink,
    )

    # Reconcile share_uplink firewall state on start. Brings sysctl
    # ip_forward + NAT MASQUERADE in line with the persisted
    # `ground_station.share_uplink` flag so reboots survive.
    try:
        from ados.services.ground_station.share_uplink_firewall import (
            reconcile_on_start as _reconcile_share_uplink,
        )
        result = await _reconcile_share_uplink()
        log.info("uplink.share_uplink_reconciled", result=result)
    except Exception as exc:
        log.warning("uplink.share_uplink_reconcile_failed", error=str(exc))

    # Wire data-cap throttle consumer. Subscribes to the router's own
    # bus and calls share_uplink_firewall.apply_throttle on each
    # DataCapState transition. Direct bus subscribe matches the pattern
    # used by CloudRelayBridge and the manager-event bridges.
    throttle_task: Optional[asyncio.Task] = None
    try:
        throttle_task = asyncio.create_task(
            _run_data_cap_throttle_consumer(router)
        )
    except Exception as exc:
        log.warning("uplink.throttle_consumer_start_failed", error=str(exc))

    await stop_event.wait()

    if throttle_task is not None:
        throttle_task.cancel()
        try:
            await throttle_task
        except (asyncio.CancelledError, Exception):
            pass

    await router.stop()
    log.info("uplink.service_stopped")


def main() -> None:
    """Systemd entry point for `ados-uplink.service`."""
    try:
        asyncio.run(_run_service())
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
