"""WFB-ng link status and control routes."""

from __future__ import annotations

import os
import subprocess
from pathlib import Path
from typing import Any

import yaml
from fastapi import APIRouter, HTTPException
from pydantic import BaseModel, Field

from ados.api.deps import get_agent_app
from ados.core.paths import CONFIG_YAML
from ados.services.wfb.channel import STANDARD_CHANNELS, get_channel

router = APIRouter()


class ChannelRequest(BaseModel):
    """Request body for channel change."""

    channel: int


class TxPowerRequest(BaseModel):
    """Request body for runtime TX power change.

    The driver applies the value via `iw dev <iface> set txpower fixed`.
    Operators override the boot default at runtime; the new value is
    persisted to /etc/ados/config.yaml so it survives a service restart.
    """

    tx_power_dbm: int = Field(..., description="Requested TX power in dBm.")


def _read_regulatory_domain() -> str:
    """Best-effort `iw reg get` first-line parse. Returns 'unknown' on failure."""
    try:
        result = subprocess.run(
            ["iw", "reg", "get"],
            capture_output=True,
            text=True,
            timeout=2,
        )
        if result.returncode != 0:
            return "unknown"
        for line in result.stdout.splitlines():
            stripped = line.strip()
            if stripped.startswith("country "):
                # Format: "country US: DFS-FCC" — keep the two-letter code.
                rest = stripped.split("country ", 1)[1]
                code = rest.split(":", 1)[0].strip()
                return code or "unknown"
            if stripped.startswith("global"):
                return "global"
        return "unknown"
    except (FileNotFoundError, subprocess.TimeoutExpired, OSError):
        return "unknown"


def _persist_tx_power(dbm: int) -> bool:
    """Atomically write `video.wfb.tx_power_dbm` to the on-disk config.

    Mirrors the tmp-write + os.replace idiom used elsewhere in the agent.
    Returns True on success, False if the file is unreadable or unwritable.
    """
    path = Path(str(CONFIG_YAML))
    try:
        data: dict[str, Any] = {}
        if path.is_file():
            with open(path, encoding="utf-8") as fh:
                loaded = yaml.safe_load(fh)
            if isinstance(loaded, dict):
                data = loaded
        video = data.get("video")
        if not isinstance(video, dict):
            video = {}
        wfb_section = video.get("wfb")
        if not isinstance(wfb_section, dict):
            wfb_section = {}
        wfb_section["tx_power_dbm"] = int(dbm)
        video["wfb"] = wfb_section
        data["video"] = video

        body = yaml.safe_dump(data, sort_keys=False, default_flow_style=False)
        path.parent.mkdir(parents=True, exist_ok=True)
        tmp_path = path.with_suffix(path.suffix + ".tmp")
        tmp_path.write_text(body, encoding="utf-8")
        os.replace(str(tmp_path), str(path))
        return True
    except (OSError, yaml.YAMLError):
        return False


@router.get("/wfb")
async def get_wfb_status():
    """Current WFB-ng link status: state, RSSI, channel, packet stats, adapter info."""
    app = get_agent_app()
    wfb = app.wfb_manager()
    cfg = app.config
    wfb_cfg = getattr(cfg.video, "wfb", None) if cfg is not None else None

    if wfb is None:
        return {
            "state": "disabled",
            "interface": "",
            "channel": 0,
            "frequency_mhz": 0,
            "bandwidth_mhz": 0,
            "adapter": {
                "driver": "",
                "chipset": "",
                "supports_monitor": False,
            },
            "rssi_dbm": -100.0,
            "noise_dbm": -95.0,
            "snr_db": 0.0,
            "packets_received": 0,
            "packets_lost": 0,
            "loss_percent": 0.0,
            "fec_recovered": 0,
            "fec_failed": 0,
            "bitrate_kbps": 0,
            "restart_count": 0,
            "samples": 0,
            "tx_power_dbm": getattr(wfb_cfg, "tx_power_dbm", None),
            "tx_power_max_dbm": getattr(wfb_cfg, "tx_power_max_dbm", None),
            "topology": getattr(wfb_cfg, "topology", None),
            "mcs_index": getattr(wfb_cfg, "mcs_index", None),
            "regulatory_domain": _read_regulatory_domain(),
        }
    status = wfb.get_status()

    # Enrich with channel frequency info
    ch_info = get_channel(status.get("channel", 0))
    if ch_info:
        status["frequency_mhz"] = ch_info.frequency_mhz
        status["bandwidth_mhz"] = ch_info.bandwidth_mhz
    else:
        status["frequency_mhz"] = 0
        status["bandwidth_mhz"] = 0

    # Live runtime TX power preferred over stored value, falling back to
    # the persisted YAML if the manager hasn't applied anything yet.
    effective = getattr(wfb, "effective_tx_power_dbm", None)
    if effective is not None:
        status["tx_power_dbm"] = effective
    elif "tx_power_dbm" not in status or status.get("tx_power_dbm") is None:
        status["tx_power_dbm"] = getattr(wfb_cfg, "tx_power_dbm", None)

    if "tx_power_max_dbm" not in status or status.get("tx_power_max_dbm") is None:
        status["tx_power_max_dbm"] = getattr(wfb_cfg, "tx_power_max_dbm", None)
    if "topology" not in status or status.get("topology") is None:
        status["topology"] = getattr(wfb_cfg, "topology", None)
    if "mcs_index" not in status or status.get("mcs_index") is None:
        status["mcs_index"] = getattr(wfb_cfg, "mcs_index", None)

    status["regulatory_domain"] = _read_regulatory_domain()

    # Enrich with adapter details if available
    adapter_info: dict[str, object] = {
        "driver": "",
        "chipset": "",
        "supports_monitor": False,
    }
    try:
        from ados.services.wfb.adapter import detect_wfb_adapters

        interface_name = status.get("interface", "")
        if interface_name:
            adapters = detect_wfb_adapters()
            for adapter in adapters:
                if adapter.interface_name == interface_name:
                    adapter_info = {
                        "driver": adapter.driver,
                        "chipset": adapter.chipset,
                        "supports_monitor": adapter.supports_monitor,
                    }
                    break
    except Exception:
        pass

    status["adapter"] = adapter_info
    return status


@router.get("/wfb/history")
async def get_wfb_history(seconds: int = 60):
    """Link quality history for the last N seconds.

    Query params:
        seconds: Number of seconds of history (default 60, max 300).
    """
    app = get_agent_app()
    wfb = app.wfb_manager()
    if wfb is None:
        return {"samples": [], "count": 0}

    seconds = min(max(seconds, 1), 300)
    history = wfb.monitor.get_history(seconds)
    return {
        "samples": [s.to_dict() for s in history],
        "count": len(history),
    }


@router.post("/wfb/channel")
async def set_wfb_channel(request: ChannelRequest):
    """Set the WFB-ng channel manually.

    Body:
        channel: Channel number (e.g. 36, 48, 149, 153, 157, 161, 165).
    """
    ch = get_channel(request.channel)
    if ch is None:
        valid = [c.channel_number for c in STANDARD_CHANNELS]
        raise HTTPException(
            status_code=400,
            detail=f"Invalid channel {request.channel}. Valid channels: {valid}",
        )

    app = get_agent_app()
    wfb = app.wfb_manager()
    if wfb is None:
        raise HTTPException(status_code=503, detail="WFB-ng service not running")

    # For the real manager, changing channel requires restart.
    # For demo, just update the stored value.
    wfb._channel = request.channel
    return {
        "status": "ok",
        "channel": request.channel,
        "frequency_mhz": ch.frequency_mhz,
    }


@router.put("/wfb/tx-power")
async def set_wfb_tx_power(request: TxPowerRequest):
    """Set the WFB-ng TX power at runtime.

    Body:
        tx_power_dbm: Requested TX power in dBm.

    Validation:
        * Refuses values below 1 dBm.
        * Refuses values above the configured `tx_power_max_dbm` ceiling.

    On accept the running manager applies the value via the kernel,
    persists `video.wfb.tx_power_dbm` to /etc/ados/config.yaml, and
    returns the requested + effective dBm reported by the driver.
    """
    app = get_agent_app()
    cfg = app.config
    wfb_cfg = getattr(cfg.video, "wfb", None) if cfg is not None else None

    requested = int(request.tx_power_dbm)
    ceiling = (
        int(getattr(wfb_cfg, "tx_power_max_dbm", 15))
        if wfb_cfg is not None
        else 15
    )

    if requested < 1:
        raise HTTPException(
            status_code=400,
            detail={"error": "below_floor", "min": 1},
        )
    if requested > ceiling:
        raise HTTPException(
            status_code=400,
            detail={"error": "above_ceiling", "max": ceiling},
        )

    wfb = app.wfb_manager()
    if wfb is None:
        raise HTTPException(status_code=503, detail="WFB-ng service not running")

    apply = getattr(wfb, "apply_tx_power", None)
    effective: int | None = None
    if callable(apply):
        try:
            effective = apply(requested)
        except Exception as exc:
            raise HTTPException(
                status_code=500,
                detail={"error": "apply_failed", "message": str(exc)},
            ) from exc

    # Persist regardless of driver outcome; on a fresh boot the manager
    # may not have an interface up yet but the operator's preference
    # should still be remembered.
    if wfb_cfg is not None:
        wfb_cfg.tx_power_dbm = requested
    _persist_tx_power(requested)

    return {
        "requested_dbm": requested,
        "effective_dbm": effective,
        "tx_power_max_dbm": ceiling,
    }


# ---------------------------------------------------------------------
# Pair lifecycle: local-radio bind, status, unpair, auto-pair toggle.
# ---------------------------------------------------------------------


def _agent_role_from_profile(profile: str) -> str:
    """Map the agent profile string to the bind-protocol role.

    Drone profile = `drone` (runs wfb_bind_server). Anything else
    (ground_station, auto-resolved-as-gs at boot) is treated as `gs`
    (runs wfb_bind_client).
    """
    return "drone" if profile == "drone" else "gs"


class LocalBindRequest(BaseModel):
    """POST body for `/wfb/pair/local-bind`."""

    role: str | None = Field(
        default=None,
        description="`drone` or `gs`. Defaults to the agent's configured profile.",
    )
    peer_device_id: str | None = Field(
        default=None,
        description="Optional peer device-id to persist with the pair state.",
    )


class AutoPairToggleRequest(BaseModel):
    """PUT body for `/wfb/pair/auto-pair`."""

    enabled: bool


@router.get("/wfb/pair")
async def get_wfb_pair_status() -> dict[str, Any]:
    """Pair-state snapshot (paired, peer device-id, fingerprint, auto-pair)."""
    app = get_agent_app()
    role = _agent_role_from_profile(app.config.agent.profile)

    from ados.services.ground_station.pair_manager import get_pair_manager

    pm = get_pair_manager()
    return await pm.status(role)


@router.post("/wfb/pair/local-bind")
async def post_wfb_pair_local_bind(request: LocalBindRequest) -> dict[str, Any]:
    """Open a local-radio bind window via the upstream wfb-ng protocol.

    Returns a session dict (`session_id`, `state`, `started_at`,
    `finished_at`, `error`, `fingerprint`, `peer_device_id`, `source`).
    The endpoint is synchronous: the entire bind protocol runs to
    completion within the request, capped at 60 seconds. Long-poll
    clients should issue this and treat the response as the terminal
    state. Concurrent calls fail-fast with a 409.
    """
    app = get_agent_app()
    role = (
        request.role
        if request.role in ("drone", "gs")
        else _agent_role_from_profile(app.config.agent.profile)
    )

    from ados.services.wfb.bind_orchestrator import (
        BindBusyError,
        BindError,
        get_bind_orchestrator,
    )

    orch = get_bind_orchestrator()
    try:
        return await orch.start_local_bind(
            role=role,
            peer_device_id=request.peer_device_id,
            source="operator",
        )
    except BindBusyError as exc:
        raise HTTPException(
            status_code=409,
            detail={"error": {"code": "E_BIND_IN_PROGRESS", "message": str(exc)}},
        ) from exc
    except BindError as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_BIND_FAILED", "message": str(exc)}},
        ) from exc


@router.get("/wfb/pair/local-bind")
async def get_wfb_pair_local_bind() -> dict[str, Any]:
    """Latest bind-session snapshot, or `{}` if none has run since boot."""
    from ados.services.wfb.bind_orchestrator import get_bind_orchestrator

    snap = await get_bind_orchestrator().status()
    return snap or {}


@router.post("/wfb/pair/unpair")
async def post_wfb_pair_unpair() -> dict[str, Any]:
    """Wipe both key files, clear pair state, restart the wfb service.

    Leaves `auto_pair_enabled = false` so the rig does not silently
    re-bind. The operator must explicitly re-arm to start auto-bind
    again.
    """
    app = get_agent_app()
    role = _agent_role_from_profile(app.config.agent.profile)

    from ados.services.ground_station.pair_manager import get_pair_manager

    pm = get_pair_manager()
    return await pm.unpair(role)


@router.put("/wfb/pair/auto-pair")
async def put_wfb_pair_auto_pair(request: AutoPairToggleRequest) -> dict[str, Any]:
    """Toggle `wfb.auto_pair_enabled`.

    Re-arming on a paired rig returns `rearm_blocked: true`; the
    operator must `unpair` first.
    """
    app = get_agent_app()
    role = _agent_role_from_profile(app.config.agent.profile)

    from ados.services.ground_station.pair_manager import get_pair_manager

    pm = get_pair_manager()
    result = await pm.set_auto_pair(bool(request.enabled), role)

    # Mirror onto the live config object so the auto_pair supervisor
    # observes the change without a config reload race.
    wfb_cfg = getattr(app.config.video, "wfb", None) if app.config is not None else None
    if wfb_cfg is not None:
        wfb_cfg.auto_pair_enabled = bool(result.get("auto_pair_enabled", False))

    return result
