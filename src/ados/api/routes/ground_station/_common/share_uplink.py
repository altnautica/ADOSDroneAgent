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


async def _resolve_active_iface() -> str | None:
    """Resolve the kernel iface of the active uplink, store-first.

    The failover loop runs in the native ``ados-net`` daemon, so the
    in-FastAPI-process ``UplinkRouter`` singleton is dead-on-read (its
    ``active_uplink`` is always ``None``). The daemon ships its selected uplink
    name as a ``net.uplink_active`` event; read that and map the uplink NAME to
    its kernel iface via the router's stateless name→iface helper (which only
    constructs the managers and reads their iface, it does not depend on the
    live failover loop). ``None`` means no active uplink could be resolved.
    """
    from ados.api.sources.network import latest_uplink_active

    active_name: str | None = None
    store = await latest_uplink_active()
    if isinstance(store, dict):
        candidate = store.get("active_uplink")
        if isinstance(candidate, str) and candidate:
            active_name = candidate

    if not active_name:
        return None

    try:
        router_ = _uplink_router()
        iface = await router_._uplink_iface(active_name)  # type: ignore[attr-defined]
        if isinstance(iface, str) and iface:
            return iface
    except Exception:
        return None
    return None


async def _apply_share_uplink(enabled: bool) -> dict[str, Any]:
    """Apply sysctl + NAT and persist firewall state across reboots.

    Delegates to ``services/ground_station/share_uplink_firewall.apply_share_uplink``
    which handles distro detection, iptables-persistent vs nftables
    fallback, atomic sysctl drop-in, and persistence of the rule set.

    The NAT MASQUERADE rule is scoped to the active uplink's kernel iface. When
    no active uplink can be resolved there is no iface to MASQUERADE on, so the
    helper returns ``applied: False`` with a short ``reason`` instead of
    reporting success against a missing iface. The contract is stable: the
    response always carries a boolean ``applied`` and, when ``applied`` is
    ``False``, a short string ``reason`` the GCS surfaces to the operator.
    """
    active_iface = await _resolve_active_iface()
    if not active_iface:
        # No active uplink → there is no iface to MASQUERADE on. Report the
        # honest not-applied result with the reason carried in both the explicit
        # `reason` field and `apply_error` (the latter is what older GCS builds
        # render as the not-applied cause).
        return {
            "applied": False,
            "reason": "no_active_uplink",
            "apply_error": "no_active_uplink",
            "backend": None,
        }

    try:
        from ados.services.ground_station.share_uplink_firewall import (
            apply_share_uplink as _apply,
        )
        result = await _apply(bool(enabled), active_iface)
    except Exception as exc:
        return {
            "applied": False,
            "reason": "firewall_helper_failed",
            "apply_error": f"firewall_helper_failed: {exc}",
            "backend": None,
        }

    applied = bool(result.get("applied", False))
    out: dict[str, Any] = {
        "applied": applied,
        "apply_error": result.get("apply_error"),
        "backend": result.get("backend"),
    }
    if not applied:
        out["reason"] = result.get("apply_error") or "apply_failed"
    return out


__all__ = [
    "_load_share_uplink_flag",
    "_persist_share_uplink_flag",
    "_apply_share_uplink",
]
