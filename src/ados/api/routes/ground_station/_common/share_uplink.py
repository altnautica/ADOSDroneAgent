"""Share-uplink read / persist / apply helpers.

The share-uplink toggle exposes the ground-station's active uplink
(WiFi client, Ethernet, modem) to AP-side clients via iptables NAT.
The authoritative source is the Pydantic-backed ``ADOSConfig`` on
disk; the legacy JSON side-file is preserved for rollback but not
written to. The ``apply`` helper delegates to the firewall helper
in the service layer, which handles distro detection and persistence.
"""

from __future__ import annotations

from typing import Any

from .managers import _uplink_router


def _load_share_uplink_flag() -> bool:
    """Read share_uplink from the Pydantic-backed ADOSConfig.

    Authoritative source is ``ADOSConfig.ground_station.share_uplink``
    (YAML). The legacy JSON side-file at ``_UI_CONFIG_PATH`` is handled
    by the one-shot migrator in ``ados.core.config.load_config()`` and
    preserved on disk.
    """
    try:
        from ados.core.config import load_config

        cfg = load_config()
        return bool(cfg.ground_station.share_uplink)
    except Exception:
        return False


def _persist_share_uplink_flag(enabled: bool) -> None:
    """Write share_uplink into the Pydantic-backed ADOSConfig on disk.

    Writes to ``/etc/ados/config.yaml`` under
    ``ground_station.share_uplink``. The legacy JSON side-file is not
    written but is preserved on disk for rollback. The pair_manager
    atomic save helper is reused so air and ground paths share one
    code path.
    """
    from ados.services.ground_station.pair_manager import (
        _load_config_dict,
        _save_config_dict,
    )

    data = _load_config_dict()
    gs_section = data.get("ground_station")
    if not isinstance(gs_section, dict):
        gs_section = {}
        data["ground_station"] = gs_section
    gs_section["share_uplink"] = bool(enabled)
    if not _save_config_dict(data):
        raise OSError("failed to persist share_uplink to /etc/ados/config.yaml")


async def _apply_share_uplink(enabled: bool) -> dict[str, Any]:
    """Apply sysctl + NAT and persist firewall state across reboots.

    Delegates to ``services/ground_station/share_uplink_firewall.apply_share_uplink``
    which handles distro detection, iptables-persistent vs nftables
    fallback, atomic sysctl drop-in, and persistence of the rule set.
    """
    active_iface: str | None = None
    try:
        router_ = _uplink_router()
        active_name = router_.active_uplink
        if active_name:
            mgr = await router_._manager_for(active_name)  # type: ignore[attr-defined]
            if mgr is not None:
                get_iface = getattr(mgr, "get_iface", None)
                if callable(get_iface):
                    active_iface = get_iface()
    except Exception:
        active_iface = None

    try:
        from ados.services.ground_station.share_uplink_firewall import (
            apply_share_uplink as _apply,
        )
        result = await _apply(bool(enabled), active_iface)
    except Exception as exc:
        return {"applied": False, "apply_error": f"firewall_helper_failed: {exc}"}

    return {
        "applied": bool(result.get("applied", False)),
        "apply_error": result.get("apply_error"),
        "backend": result.get("backend"),
    }


__all__ = [
    "_load_share_uplink_flag",
    "_persist_share_uplink_flag",
    "_apply_share_uplink",
]
