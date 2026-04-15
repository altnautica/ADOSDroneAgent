"""Ground-station profile routes (DEC-112, MSN-024 Phase 0).

These endpoints are gated on `config.agent.profile == "ground_station"`.
Agents running the default drone profile return 404 with code
`E_PROFILE_MISMATCH` per the spec at
`product/specs/ados-ground-agent/11-agent-api-surface.md`.

Phase 0 ships a minimal surface: status snapshot, WFB radio get, WFB radio
put. Most link and client fields are stubs with safe defaults while the
service-layer modules under `ados.services.ground_station` come online in
parallel.
"""

from __future__ import annotations

from typing import Any

from fastapi import APIRouter, HTTPException
from pydantic import BaseModel

from ados.api.deps import get_agent_app

router = APIRouter(prefix="/v1/ground-station", tags=["ground-station"])


def _require_ground_profile() -> Any:
    """Gate: return the agent app if profile is ground_station, else 404."""
    app = get_agent_app()
    profile = getattr(app.config.agent, "profile", "auto")
    if profile != "ground_station":
        raise HTTPException(
            status_code=404,
            detail={"error": {"code": "E_PROFILE_MISMATCH"}},
        )
    return app


def _save_config(app: Any) -> None:
    """Persist the agent config back to disk.

    Phase 0 uses a best-effort save: if the config object or loader exposes
    a save helper we call it, otherwise we log and skip. The caller still
    sees the live in-memory change.
    """
    saver = getattr(app, "save_config", None)
    if callable(saver):
        try:
            saver()
            return
        except Exception:
            pass
    cfg_save = getattr(app.config, "save", None)
    if callable(cfg_save):
        try:
            cfg_save()
        except Exception:
            pass


@router.get("/status")
async def get_ground_station_status() -> dict[str, Any]:
    """Full ground-station snapshot.

    Phase 0: link metrics and clients are stubs. Real values land when
    `ados.services.ground_station.wfb_rx` and the USB gadget manager are
    wired through the supervisor IPC.
    """
    app = _require_ground_profile()

    wfb_cfg = getattr(app.config, "wfb", None)
    channel = getattr(wfb_cfg, "channel", 0) if wfb_cfg is not None else 0

    return {
        "profile": "ground-station",
        "paired_drone": None,
        "link": {
            "rssi_dbm": None,
            "bitrate_mbps": None,
            "fec_rec": None,
            "fec_lost": None,
            "channel": channel,
        },
        "uplink": {
            "active": None,
            "available": [],
        },
        "clients": [],
        "recording": False,
    }


class WfbUpdate(BaseModel):
    """PUT body for the ground-station WFB radio config."""

    channel: int | None = None
    bitrate_profile: str | None = None
    fec: str | None = None


def _read_wfb_view(app: Any) -> dict[str, Any]:
    """Pull the three Phase 0 fields from agent config with safe fallbacks."""
    wfb_cfg = getattr(app.config, "wfb", None)
    return {
        "channel": getattr(wfb_cfg, "channel", 0) if wfb_cfg is not None else 0,
        "bitrate_profile": getattr(wfb_cfg, "bitrate_profile", "default")
        if wfb_cfg is not None
        else "default",
        "fec": getattr(wfb_cfg, "fec", "8/12") if wfb_cfg is not None else "8/12",
    }


@router.get("/wfb")
async def get_ground_station_wfb() -> dict[str, Any]:
    """Current radio config as stored in agent config."""
    app = _require_ground_profile()
    return _read_wfb_view(app)


@router.put("/wfb")
async def put_ground_station_wfb(update: WfbUpdate) -> dict[str, Any]:
    """Update channel, bitrate profile, or FEC and persist.

    Phase 0 writes to the in-memory config and best-effort saves to disk.
    A live radio restart is the job of the ground-station wfb_rx service
    (Second Violins) and is not performed here.
    """
    app = _require_ground_profile()

    wfb_cfg = getattr(app.config, "wfb", None)
    if wfb_cfg is None:
        raise HTTPException(
            status_code=503,
            detail={"error": {"code": "E_WFB_CONFIG_MISSING"}},
        )

    if update.channel is not None:
        if hasattr(wfb_cfg, "channel"):
            setattr(wfb_cfg, "channel", update.channel)
    if update.bitrate_profile is not None:
        if hasattr(wfb_cfg, "bitrate_profile"):
            setattr(wfb_cfg, "bitrate_profile", update.bitrate_profile)
    if update.fec is not None:
        if hasattr(wfb_cfg, "fec"):
            setattr(wfb_cfg, "fec", update.fec)

    _save_config(app)
    return _read_wfb_view(app)
