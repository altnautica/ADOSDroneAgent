"""Lazy-import singletons for the live ground-station service managers.

Each helper takes no args and returns the relevant manager. Lazy
imports keep the route module loadable without the service-layer
dependencies wired up — useful during testing and during early-boot
ordering.

These are the canonical monkeypatch targets used by route tests
(``monkeypatch.setattr(gs, "_pair_manager", ...)``). The package-level
re-export in ``ground_station/__init__.py`` ensures that pattern
keeps working after the package split.
"""

from __future__ import annotations

from typing import Any


def _hostapd_manager(app: Any) -> Any:
    """Construct a HostapdManager keyed off the running agent config."""
    from ados.services.ground_station.hostapd_manager import HostapdManager

    device_id = getattr(app.config.agent, "device_id", "unknown")
    hotspot = getattr(app.config.network, "hotspot", None)

    ssid_override: str | None = None
    if hotspot is not None:
        configured = getattr(hotspot, "ssid", "") or ""
        if (
            configured
            and "{device_id}" not in configured
            and configured.startswith("ADOS-GS-")
        ):
            ssid_override = configured

    channel = int(getattr(hotspot, "channel", 6)) if hotspot is not None else 6

    mgr = HostapdManager(
        device_id=device_id,
        ssid=ssid_override,
        channel=channel,
    )
    # Load the persisted passphrase so status() reports a stable SSID + key.
    try:
        mgr.ensure_passphrase()
    except Exception:
        pass
    return mgr


def _pair_manager() -> Any:
    """Return the process-wide PairManager. Lazy import so route module loads without it."""
    from ados.services.ground_station.pair_manager import get_pair_manager

    return get_pair_manager()


def _ethernet_mgr() -> Any:
    from ados.services.ground_station.ethernet_manager import (
        get_ethernet_manager,
    )

    return get_ethernet_manager()


def _wifi_client_manager() -> Any:
    from ados.services.ground_station.wifi_client_manager import (
        get_wifi_client_manager,
    )

    return get_wifi_client_manager()


def _modem_mgr() -> Any:
    from ados.services.ground_station.modem_manager import get_modem_manager

    return get_modem_manager()


def _uplink_router() -> Any:
    from ados.services.ground_station.uplink import get_uplink_router

    return get_uplink_router()


def _input_manager() -> Any:
    """Lazy import helper for the InputManager singleton."""
    from ados.services.ground_station.input_manager import get_input_manager

    return get_input_manager()


def _pic_arbiter() -> Any:
    """Lazy import helper for the PicArbiter singleton."""
    from ados.services.ground_station.pic_arbiter import get_pic_arbiter

    return get_pic_arbiter()


__all__ = [
    "_hostapd_manager",
    "_pair_manager",
    "_ethernet_mgr",
    "_wifi_client_manager",
    "_modem_mgr",
    "_uplink_router",
    "_input_manager",
    "_pic_arbiter",
]
