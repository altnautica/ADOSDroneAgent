"""View aggregators that compose the live status JSON.

Each helper returns a dict shape consumed by one or more of the
OLED dashboard, the GCS Hardware tab, the public status route, and
the WHEP / mesh sub-routers. The functions read from agent config,
the WFB stats file, and the live service managers.
"""

from __future__ import annotations

from typing import Any

from .managers import _hostapd_manager


def _link_view(app: Any) -> dict[str, Any]:
    """Live radio link view sourced from /run/ados/wfb-stats.json.

    The OLED dashboard radio-link tile, the OLED link-screen, and the
    GCS Hardware tab all read this block. Static config (channel,
    topology, tx_power_max) merges with the live snapshot the wfb
    manager writes once per stats interval. Falls back to None /
    zeros if the file isn't there yet (fresh boot before first PKT
    line) or is stale (manager dead / radio interface gone).
    """
    import json as _json
    import time as _time

    from ados.core.paths import WFB_STATS_JSON

    wfb_cfg = getattr(app.config, "wfb", None)
    config_channel = (
        getattr(wfb_cfg, "channel", None) if wfb_cfg is not None else None
    )

    # Defaults — what the dashboard renders before the wfb manager
    # has written its first snapshot.
    base: dict[str, Any] = {
        "rssi_dbm": None,
        "bitrate_mbps": None,
        # Mirror of bitrate_mbps in kbps so consumers that key on the
        # canonical producer field (``bitrate_kbps`` from
        # ``LinkStats.to_dict``) read populated values without a
        # per-page rename. See the LCD link stats page fallback path.
        "bitrate_kbps": None,
        "fec_recovered": 0,
        "fec_lost": 0,
        # Mirror of fec_lost under the producer key. Same rationale.
        "fec_failed": 0,
        "channel": config_channel,
        "snr_db": None,
        "noise_dbm": None,
        "packets_received": 0,
        "packets_lost": 0,
        "loss_percent": None,
        "tx_power_dbm": getattr(wfb_cfg, "tx_power_dbm", None)
        if wfb_cfg is not None
        else None,
        "state": "connecting",
    }

    try:
        st = WFB_STATS_JSON.stat()
        age_s = _time.time() - st.st_mtime
        with open(WFB_STATS_JSON) as f:
            payload = _json.load(f)
        if not isinstance(payload, dict):
            return base
        rssi = payload.get("rssi_dbm")
        bitrate_kbps = payload.get("bitrate_kbps")
        bitrate_mbps = (
            round(bitrate_kbps / 1000.0, 2)
            if isinstance(bitrate_kbps, (int, float))
            else None
        )
        # Live snapshot wins. Channel from the file (what the manager
        # actually applied) wins over the disk config when present.
        fec_failed = int(payload.get("fec_failed") or 0)
        merged: dict[str, Any] = {
            **base,
            "rssi_dbm": rssi if isinstance(rssi, (int, float)) else None,
            "bitrate_mbps": bitrate_mbps,
            "bitrate_kbps": int(bitrate_kbps)
            if isinstance(bitrate_kbps, (int, float))
            else None,
            "fec_recovered": int(payload.get("fec_recovered") or 0),
            "fec_lost": fec_failed,
            "fec_failed": fec_failed,
            "channel": payload.get("channel") or config_channel,
            "snr_db": payload.get("snr_db"),
            "noise_dbm": payload.get("noise_dbm"),
            "packets_received": int(payload.get("packets_received") or 0),
            "packets_lost": int(payload.get("packets_lost") or 0),
            "loss_percent": payload.get("loss_percent"),
            "tx_power_dbm": payload.get("tx_power_dbm")
            if payload.get("tx_power_dbm") is not None
            else base["tx_power_dbm"],
            "state": payload.get("state") or "connecting",
        }
        # 10 s mtime ceiling — over that, the snapshot is suspect.
        # Mark state="stale" so the LCD can render a yellow badge.
        if age_s > 10.0:
            merged["state"] = "stale"
        return merged
    except (FileNotFoundError, ValueError, OSError):
        return base


def _network_view(app: Any) -> dict[str, Any]:
    """AP-only view for the OLED status schema."""
    ap_ssid: str | None = None
    ap_ip: str | None = None
    try:
        mgr = _hostapd_manager(app)
        st = mgr.status()
        ap_ssid = st.get("ssid")
        ap_ip = st.get("gateway")
    except Exception:
        pass
    return {
        "ap_ssid": ap_ssid,
        "ap_ip": ap_ip,
        "usb_ip": None,
        "uplink_type": None,
        "uplink_reachable": False,
    }


def _read_wfb_view(app: Any) -> dict[str, Any]:
    # WfbConfig lives at app.config.video.wfb, not app.config.wfb. The
    # earlier lookup at the root always returned None and the GET view
    # silently returned defaults (channel=0, profile=default, fec=8/12)
    # regardless of what the operator actually configured.
    video_cfg = getattr(app.config, "video", None)
    wfb_cfg = getattr(video_cfg, "wfb", None) if video_cfg is not None else None
    return {
        "channel": getattr(wfb_cfg, "channel", 0) if wfb_cfg is not None else 0,
        "bitrate_profile": getattr(wfb_cfg, "bitrate_profile", "default")
        if wfb_cfg is not None
        else "default",
        "fec": getattr(wfb_cfg, "fec", "8/12") if wfb_cfg is not None else "8/12",
    }


def _ap_view(app: Any) -> dict[str, Any]:
    try:
        mgr = _hostapd_manager(app)
        st = mgr.status()
        return {
            "enabled": bool(st.get("running", False)),
            "running": bool(st.get("running", False)),
            "ssid": st.get("ssid"),
            "channel": st.get("channel"),
            "interface": st.get("interface"),
            "gateway": st.get("gateway"),
            "connected_clients": st.get("connected_clients", []),
        }
    except Exception:
        hotspot = getattr(app.config.network, "hotspot", None)
        return {
            "enabled": False,
            "running": False,
            "ssid": getattr(hotspot, "ssid", None) if hotspot is not None else None,
            "channel": getattr(hotspot, "channel", None)
            if hotspot is not None
            else None,
            "interface": None,
            "gateway": None,
            "connected_clients": [],
        }


async def _wifi_client_view() -> dict[str, Any]:
    """Surface WifiClientManager status + enabled_on_boot."""
    try:
        from ados.services.ground_station.wifi_client_manager import (
            get_wifi_client_manager,
        )

        mgr = get_wifi_client_manager()
        st = await mgr.status()
        cfg = mgr._load_client_config()  # internal helper, ok for a view
        return {
            "enabled_on_boot": bool(cfg.get("enabled_on_boot", False)),
            "connected": bool(st.get("connected", False)),
            "ssid": st.get("ssid"),
            "signal": st.get("signal"),
            "ip": st.get("ip"),
        }
    except Exception:
        return {
            "enabled_on_boot": False,
            "connected": False,
            "ssid": None,
            "signal": None,
            "ip": None,
        }


async def _ethernet_view() -> dict[str, Any]:
    try:
        from ados.services.ground_station.ethernet_manager import (
            get_ethernet_manager,
        )

        st = await get_ethernet_manager().status()
        return {
            "link": bool(st.get("link", False)),
            "speed_mbps": st.get("speed_mbps"),
            "ip": st.get("ip"),
            "gateway": st.get("gateway"),
        }
    except Exception:
        return {"link": False, "speed_mbps": None, "ip": None, "gateway": None}


async def _modem_view() -> dict[str, Any]:
    try:
        from ados.services.ground_station.modem_manager import (
            get_modem_manager,
        )

        mgr = get_modem_manager()
        st = await mgr.status()
        usage = await mgr.data_usage()
        cfg = getattr(mgr, "_config", {}) or {}

        cap_gb = cfg.get("cap_gb")
        try:
            cap_mb = int(float(cap_gb) * 1024) if cap_gb is not None else 0
        except (TypeError, ValueError):
            cap_mb = 0
        total_bytes = int(usage.get("total_bytes", 0) or 0)
        data_used_mb = int(total_bytes / (1024 * 1024)) if total_bytes else 0
        percent = (data_used_mb / cap_mb * 100.0) if cap_mb else 0.0

        return {
            "enabled": bool(cfg.get("enabled", False)),
            "connected": bool(st.get("connected", False)),
            "iface": st.get("iface"),
            "ip": st.get("ip"),
            "signal_quality": st.get("signal_quality"),
            "technology": st.get("technology"),
            "apn": st.get("apn") or cfg.get("apn"),
            "operator": st.get("operator"),
            "data_used_mb": data_used_mb,
            "cap_mb": cap_mb,
            "percent": round(percent, 2),
            "state": "connected" if st.get("connected") else "disconnected",
        }
    except Exception:
        return {
            "enabled": False,
            "connected": False,
            "iface": None,
            "ip": None,
            "signal_quality": None,
            "technology": None,
            "apn": None,
            "operator": None,
            "data_used_mb": 0,
            "cap_mb": 0,
            "percent": 0.0,
            "state": "unknown",
        }


def _router_state_view() -> dict[str, Any]:
    """Active uplink + priority list from UplinkRouter singleton."""
    try:
        from ados.services.ground_station.uplink_router import get_uplink_router

        router = get_uplink_router()
        return {
            "active_uplink": router.active_uplink,
            "priority": list(router.get_priority()),
        }
    except Exception:
        return {"active_uplink": None, "priority": []}


__all__ = [
    "_link_view",
    "_network_view",
    "_read_wfb_view",
    "_ap_view",
    "_wifi_client_view",
    "_ethernet_view",
    "_modem_view",
    "_router_state_view",
]
