"""Ground-station /status endpoint.

Returns the OLED-aligned snapshot used by the ground node UI and the
GCS Hardware tab.
"""

from __future__ import annotations

import json
from typing import Any

from fastapi import APIRouter

from ados.api.routes import ground_station as _gs
from ados.core.paths import MESH_STATE_JSON, PROFILE_CONF


router = APIRouter(prefix="/v1/ground-station", tags=["ground-station"])


@router.get("/status")
async def get_ground_station_status() -> dict[str, Any]:
    """Full ground-station snapshot aligned with the OLED schema.

    Matches the fields the OLED service polls at 1 Hz. Fields not yet
    sourced (paired drone telemetry, gcs clients, uplink) return None.
    """
    app = _gs._require_ground_profile()

    # Surface the current pair key fingerprint alongside the paired
    # drone id. Source is PairManager.status().
    paired_drone_id: str | None = None
    key_fingerprint: str | None = None
    try:
        pair_status = await _gs._pair_manager().status()
        if pair_status.get("paired"):
            paired_drone_id = pair_status.get("paired_drone_id")
        key_fingerprint = pair_status.get("key_fingerprint")
    except Exception:
        pass

    # Role snapshot. Source of truth for the OLED Mesh submenu
    # visibility and for the GCS TopBar role badge.
    #
    # `current` reads `/etc/ados/mesh/role` (authoritative across
    # restarts and during a transition). `configured` reads the
    # Pydantic config value. They diverge briefly during early boot
    # before `Supervisor._apply_ground_station_role` runs, and during
    # a live role transition. Clients that drive state-machine
    # decisions should always prefer `current`; `configured` is shown
    # to the operator as "intended role" when it differs from active.
    role_block: dict[str, Any] = {
        "current": "direct",
        "configured": "direct",
        "supported": ["direct", "relay", "receiver"],
        "mesh_capable": False,
    }
    try:
        from ados.services.ground_station.role_manager import get_current_role
        role_block["current"] = get_current_role()
    except Exception:
        pass
    try:
        role_block["configured"] = getattr(
            getattr(app.config, "ground_station", None), "role", "direct"
        )
    except Exception:
        pass
    profile_conf = _gs._read_yaml_or_empty(PROFILE_CONF)
    role_block["mesh_capable"] = bool(profile_conf.get("mesh_capable", False))

    # Mesh snapshot. Populated only when a relay or receiver node has an
    # active mesh. Direct nodes get an empty dict so the OLED and GCS can
    # feature-detect without extra round-trips.
    mesh_block: dict[str, Any] = {}
    try:
        snap_path = MESH_STATE_JSON
        if role_block["current"] in ("relay", "receiver") and snap_path.is_file():
            snap = json.loads(snap_path.read_text(encoding="utf-8"))
            mesh_block = {
                "up": bool(snap.get("up", False)),
                "peer_count": len(snap.get("neighbors", [])),
                "selected_gateway": snap.get("selected_gateway"),
                "partition": bool(snap.get("partition", False)),
                "mesh_id": snap.get("mesh_id"),
            }
    except Exception:
        pass

    return {
        "profile": "ground_station",
        "paired_drone": {
            "device_id": paired_drone_id,
            "key_fingerprint": key_fingerprint,
            "fc_mode": None,
            "battery_pct": None,
            "gps_sats": None,
        },
        "link": _gs._link_view(app),
        "gcs": {"clients": [], "pic_id": None},
        "network": _gs._network_view(app),
        "system": _gs._system_snapshot(),
        "recording": False,
        "role": role_block,
        "mesh": mesh_block,
    }
