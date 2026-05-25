"""Encoder + radio config + closed-loop snapshot routes.

* ``GET /video/config`` — composite live snapshot covering radio,
  encoder, adaptive bitrate ladder, frequency-hop supervisor.
* ``POST /video/config`` — apply partial updates to bitrate / FEC /
  MCS / auto / pinned tier. Each field is independent so a partial
  request leaves the rest untouched.

Both routes read from the in-process accessor when available and fall
back to the state files the wfb-side controllers persist under
``/run/ados/`` so the API can serve sensible data even when the
controllers live in a different process.
"""

from __future__ import annotations

from typing import Any

from fastapi import APIRouter

from ados.api.deps import get_agent_app

from ._common import VideoConfigBody

router = APIRouter()


def _read_state_file(path: str) -> dict[str, Any] | None:
    """Read a JSON snapshot file written by a wfb-side controller.

    The BitrateController and HopSupervisor run inside ados-wfb
    (separate process from ados-api in production multi-process
    systemd). They persist their snapshots to /run/ados/*.json
    every ~5 s; this reader pulls whichever file is asked for.
    Returns None on any read or parse failure so the caller can
    fall back to defaults.
    """
    try:
        import json
        from pathlib import Path

        p = Path(path)
        if not p.is_file():
            return None
        blob = json.loads(p.read_text())
        return blob if isinstance(blob, dict) else None
    except (OSError, ValueError):
        return None


def _bitrate_controller_snapshot(app: Any) -> dict[str, Any] | None:
    """Read the BitrateController snapshot.

    First tries the in-process accessor (single-process mode,
    bench dev). Falls back to the state file the controller
    persists at /run/ados/bitrate-controller.json (production
    multi-process). Returns None when neither path yields data.
    """
    getter = getattr(app, "bitrate_controller", None)
    if callable(getter):
        ctrl = getter()
        if ctrl is not None:
            snap_fn = getattr(ctrl, "snapshot", None)
            if callable(snap_fn):
                try:
                    return snap_fn()
                except Exception:  # noqa: BLE001
                    pass
    from ados.core.paths import BITRATE_CONTROLLER_JSON

    return _read_state_file(str(BITRATE_CONTROLLER_JSON))


def _link_snapshot(app: Any, wfb_cfg: Any) -> dict[str, Any]:
    """Live radio-link liveness fields for the GCS Video Link panel.

    The panel polls only ``GET /video/config`` and reads ``config.link.*``
    to render its liveness UI. Prefer the in-process WfbManager status
    (drone, single-process bench). Fall back to the wfb-stats snapshot
    file the radio managers persist to ``/run/ados/wfb-stats.json`` so
    the ground-station profile (whose receive manager lives in a
    separate process) still serves live values. Returns a stable shape
    with ``None`` placeholders so the panel never sees missing keys.
    """
    fields = (
        "tx_bytes_per_s",
        "valid_rx_packets_per_s",
        "video_inbound_bytes_per_s",
        "rx_silent_seconds",
        "channel_locked",
        "acquire_state",
        "channel",
    )
    link: dict[str, Any] = {key: None for key in fields}

    status: dict[str, Any] | None = None
    wfb_mgr = app.wfb_manager() if hasattr(app, "wfb_manager") else None
    if wfb_mgr is not None and hasattr(wfb_mgr, "get_status"):
        try:
            status = wfb_mgr.get_status()
        except Exception:  # noqa: BLE001
            status = None
    if status is None:
        # Ground-station path (and any other profile where the manager
        # lives in a sibling process): the radio manager mirrors its
        # snapshot to the shared stats file once per stats interval.
        from ados.core.paths import WFB_STATS_JSON

        status = _read_state_file(str(WFB_STATS_JSON))

    if isinstance(status, dict):
        for key in fields:
            if status.get(key) is not None:
                link[key] = status[key]

    # Channel falls back to the configured value so the panel always has
    # a number even before the first stats line lands.
    if link["channel"] is None and wfb_cfg is not None:
        link["channel"] = getattr(wfb_cfg, "channel", None)
    return link


def _hop_supervisor_snapshot(app: Any) -> dict[str, Any] | None:
    """Read the HopSupervisor snapshot.

    Same dual-path pattern as the bitrate controller: prefer
    the in-process accessor, fall back to the state file at
    /run/ados/hop-supervisor.json.
    """
    getter = getattr(app, "hop_supervisor", None)
    if callable(getter):
        sup = getter()
        if sup is not None:
            snap_fn = getattr(sup, "snapshot", None)
            if callable(snap_fn):
                try:
                    return snap_fn()
                except Exception:  # noqa: BLE001
                    pass
    from ados.core.paths import HOP_SUPERVISOR_JSON

    return _read_state_file(str(HOP_SUPERVISOR_JSON))


@router.get("/video/config")
async def get_video_config() -> dict[str, Any]:
    """Live snapshot of the adaptive bitrate + FEC + radio config.

    Combines the static wfb config (channel, mcs, fec_k/fec_n
    persisted to /etc/ados/config.yaml) with the dynamic ladder
    state from the BitrateController. Shape is stable enough that
    the GCS Video Link panel can render its sparklines without a
    schema migration when an additional metric is added.
    """
    app = get_agent_app()
    cfg = app.config
    wfb_cfg = getattr(cfg.video, "wfb", None) if cfg is not None else None
    camera_cfg = getattr(cfg.video, "camera", None) if cfg is not None else None

    radio = {
        "channel": getattr(wfb_cfg, "channel", None) if wfb_cfg else None,
        "band": getattr(wfb_cfg, "band", None) if wfb_cfg else None,
        "mcs_index": getattr(wfb_cfg, "mcs_index", None) if wfb_cfg else None,
        "fec_k": getattr(wfb_cfg, "fec_k", None) if wfb_cfg else None,
        "fec_n": getattr(wfb_cfg, "fec_n", None) if wfb_cfg else None,
        "tx_power_dbm": (
            getattr(wfb_cfg, "tx_power_dbm", None) if wfb_cfg else None
        ),
        "preset": getattr(wfb_cfg, "wfb_link_preset", None) if wfb_cfg else None,
    }
    encoder = {
        "bitrate_kbps": (
            getattr(camera_cfg, "bitrate_kbps", None) if camera_cfg else None
        ),
        "width": getattr(camera_cfg, "width", None) if camera_cfg else None,
        "height": getattr(camera_cfg, "height", None) if camera_cfg else None,
        "fps": getattr(camera_cfg, "fps", None) if camera_cfg else None,
        "codec": getattr(camera_cfg, "codec", None) if camera_cfg else None,
    }
    adaptive = {
        "available": (
            getattr(wfb_cfg, "adaptive_bitrate_enabled", False)
            if wfb_cfg else False
        ),
    }
    snap = _bitrate_controller_snapshot(app)
    if snap is not None:
        adaptive.update(snap)

    # Hop supervisor snapshot: drone-side periodic + reactive
    # frequency hopper. Drives the GCS ChannelHistoryChart.
    # Falls back to a minimal stub on incapable rigs (e.g. GS
    # profile, which spawns the listener only, no supervisor)
    # so the GCS chart can render an "armed but quiet" state.
    hop_snap = _hop_supervisor_snapshot(app)
    if hop_snap is None:
        hopping = {
            "enabled": (
                getattr(wfb_cfg, "auto_hop_enabled", False)
                if wfb_cfg else False
            ),
            "band": getattr(wfb_cfg, "band", None) if wfb_cfg else None,
            "hop_period_seconds": (
                getattr(wfb_cfg, "hop_period_seconds", None)
                if wfb_cfg else None
            ),
            "history": [],
            "last_hop_at": 0.0,
        }
    else:
        hopping = hop_snap

    # Live radio-link liveness block. The GCS Video Link panel polls
    # only this endpoint and reads config.link.* to drive its liveness
    # UI (throughput, valid-decode rate, channel lock, acquisition
    # state). Without this block the panel renders dead.
    link = _link_snapshot(app, wfb_cfg)

    return {
        "radio": radio,
        "encoder": encoder,
        "adaptive": adaptive,
        "hopping": hopping,
        "link": link,
    }


@router.post("/video/config")
async def set_video_config(body: VideoConfigBody) -> dict[str, Any]:
    """Apply zero or more video / radio tuning knobs.

    Each field is optional and applied independently. Returns the
    same shape as GET /video/config so the GCS can refresh its
    local state from a single response. Fields that the agent
    couldn't apply (e.g. wfb_manager is None in this process)
    surface in ``warnings`` so a partial success is visible.
    """
    app = get_agent_app()
    wfb_mgr = app.wfb_manager() if hasattr(app, "wfb_manager") else None
    pipeline = app.video_pipeline() if hasattr(app, "video_pipeline") else None
    ctrl_getter = getattr(app, "bitrate_controller", None)
    ctrl = ctrl_getter() if callable(ctrl_getter) else None

    warnings: list[str] = []

    # Bitrate: pipeline-side restart. Skip if pipeline not in process.
    if body.bitrate_kbps is not None:
        if pipeline is not None and hasattr(pipeline, "set_video_bitrate"):
            ok = await pipeline.set_video_bitrate(body.bitrate_kbps)
            if not ok:
                warnings.append("set_video_bitrate_failed")
        else:
            warnings.append("video_pipeline_not_in_process")

    # FEC: stop-then-start wfb_tx. Skip if wfb not in process.
    if body.fec_k is not None or body.fec_n is not None:
        if wfb_mgr is not None and hasattr(wfb_mgr, "set_fec"):
            cfg = app.config.video.wfb
            new_k = body.fec_k if body.fec_k is not None else cfg.fec_k
            new_n = body.fec_n if body.fec_n is not None else cfg.fec_n
            ok = await wfb_mgr.set_fec(new_k, new_n)
            if not ok:
                warnings.append("set_fec_failed")
        else:
            warnings.append("wfb_manager_not_in_process")

    # MCS
    if body.mcs is not None:
        if wfb_mgr is not None and hasattr(wfb_mgr, "set_mcs"):
            ok = await wfb_mgr.set_mcs(body.mcs)
            if not ok:
                warnings.append("set_mcs_failed")
        else:
            warnings.append("wfb_manager_not_in_process")

    # Controller toggles
    if ctrl is not None:
        if body.auto is not None:
            try:
                ctrl.set_auto(body.auto)
            except Exception as exc:  # noqa: BLE001
                warnings.append(f"set_auto_failed:{exc}")
        if body.tier_idx is not None:
            try:
                ok = await ctrl.set_manual_tier(body.tier_idx)
                if not ok:
                    warnings.append("set_manual_tier_failed")
            except Exception as exc:  # noqa: BLE001
                warnings.append(f"set_manual_tier_failed:{exc}")
    elif body.auto is not None or body.tier_idx is not None:
        warnings.append("bitrate_controller_not_in_process")

    response = await get_video_config()
    response["warnings"] = warnings
    return response


__all__ = [
    "router",
    "_read_state_file",
    "_bitrate_controller_snapshot",
    "_hop_supervisor_snapshot",
    "_link_snapshot",
    "get_video_config",
    "set_video_config",
]
