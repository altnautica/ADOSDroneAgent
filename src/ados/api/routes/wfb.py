"""WFB-ng link status and control routes."""

from __future__ import annotations

from fastapi import APIRouter, HTTPException
from pydantic import BaseModel

from ados.api.deps import get_agent_app
from ados.services.wfb.channel import STANDARD_CHANNELS, get_channel

router = APIRouter()


class ChannelRequest(BaseModel):
    """Request body for channel change."""

    channel: int


@router.get("/wfb")
async def get_wfb_status():
    """Current WFB-ng link status: state, RSSI, channel, packet stats, adapter info."""
    app = get_agent_app()
    wfb = getattr(app, "_wfb_manager", None)
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
    wfb = getattr(app, "_wfb_manager", None)
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
    wfb = getattr(app, "_wfb_manager", None)
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
