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


# The `adaptive` block's live fields, read out of wfb-stats.json. The radio
# writes all of these on every tick. `available` is seeded from config
# separately and is not in this list. Mirrored by ADAPTIVE_KEYS in the native
# route; tests/test_adaptive_keys_agree_across_transports.py compares the two
# constants, because the api-conformance harness diffs live RESPONSES on a rig
# and would not catch a key added to one side in a pull request.
_ADAPTIVE_KEYS = (
    "adaptive_bitrate_enabled",
    "recommended_bitrate_kbps",
    "encoder_bitrate_kbps",
    "recommended_tier_idx",
    "link_preset",
    "mcs_index",
    "mcs_ladder_cap",
    "fec_k",
    "fec_n",
)


def _bitrate_controller_snapshot(app: Any) -> dict[str, Any] | None:
    """The live adaptive-controller state, from ``wfb-stats.json``.

    Sourced from the wfb stats sidecar, which the radio rewrites on every tick.
    This used to read ``/run/ados/bitrate-controller.json`` -- a file with no
    writer, because its only producer was a ``BitrateController`` that is never
    instantiated anywhere. The merge could therefore never fire, and the
    ``adaptive`` block was permanently just ``{"available": <config flag>}``
    while the real controller state sat unread in the sidecar next to it.

    Returns None when the sidecar is absent or carries none of these keys, so
    the caller keeps its config-seeded stub.
    """
    from ados.core.paths import WFB_STATS_JSON

    # Gated on the same freshness ceiling the `link` block applies to this same
    # file. A stale snapshot's controller state describes a radio that may be
    # dead now: a frozen ``adaptive_bitrate_enabled: True`` beside a frozen
    # recommended rung reads as a live adapting link. Absent and stale both
    # degrade to the config stub rather than to a last-known value.
    age_s = _stats_age_seconds(str(WFB_STATS_JSON))
    if age_s is None or age_s > _LINK_STALE_AFTER_S:
        return None
    blob = _read_state_file(str(WFB_STATS_JSON))
    if not blob:
        return None
    live = {k: blob[k] for k in _ADAPTIVE_KEYS if k in blob}
    return live or None


# Beyond this age the wfb-stats snapshot can no longer describe the link
# NOW, so the received-side verdict reads None. The same 10 s ceiling
# /api/wfb uses to flip its ``state`` to "stale".
_LINK_STALE_AFTER_S = 10.0


def _stats_age_seconds(path: str) -> float | None:
    """Age of the stats snapshot in seconds, or None when it is absent or
    its mtime is unreadable. Drives the staleness gate on the verdict."""
    try:
        import time
        from pathlib import Path

        return time.time() - Path(path).stat().st_mtime
    except OSError:
        return None


def _rf_unverified(status: dict[str, Any] | None, *, fresh: bool) -> bool | None:
    """The ``rf_unverified`` verdict for the link block, or None when it
    cannot be sourced honestly.

    The radio owns this boolean (an advancing transmit counter with no
    confirmed reception inside the proof grace window) and both sidecar
    writers carry it, so this block forwards the authoritative value
    instead of leaving the panel to re-derive it from the liveness
    counters beside it. It is the other half of ``channel_locked`` in the
    same block: locked is true once a verified return signal was heard,
    ``rf_unverified`` is true when the transmit counter advances while
    none has been.

    None — never a confident False — when the key is absent or
    non-boolean (a sidecar written before the field existed, or a garbled
    body) or when the snapshot is older than the staleness ceiling,
    because a reading that old cannot say whether the radio is unverified
    NOW, and a stale False is exactly the healthy-looking dead link this
    field exists to expose.
    """
    if not fresh:
        return None
    value = (status or {}).get("rf_unverified")
    return value if isinstance(value, bool) else None


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
    # The received-side verdict is always present too, None until it can be
    # sourced honestly — it is gated on type and freshness rather than
    # merged verbatim, so it is seeded outside the loop below.
    link["rf_unverified"] = None

    # The radio mirrors its snapshot to the shared stats file once per stats
    # interval; only a fresh file describes the link now.
    from ados.core.paths import WFB_STATS_JSON

    status = _read_state_file(str(WFB_STATS_JSON))
    age_s = _stats_age_seconds(str(WFB_STATS_JSON))
    fresh = age_s is not None and age_s <= _LINK_STALE_AFTER_S

    if isinstance(status, dict):
        for key in fields:
            if status.get(key) is not None:
                link[key] = status[key]
        link["rf_unverified"] = _rf_unverified(status, fresh=fresh)

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
    from ados.core.paths import HOP_SUPERVISOR_JSON

    return _read_state_file(str(HOP_SUPERVISOR_JSON))


async def get_video_config() -> dict[str, Any]:
    """Live snapshot of the adaptive bitrate + FEC + radio config.

    Served natively by the front; retained here as the helper the
    config-write path and the package re-export call to read the
    current config back.
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

    Each field is optional and applied independently. Returns the same shape
    as GET /video/config so the GCS can refresh its local state from a single
    response. The FEC / MCS / preset / link-tier knobs go to the native
    radio's command socket; a knob that could not be applied is named in
    ``warnings`` so a partial success is visible.
    """
    app = get_agent_app()
    warnings: list[str] = []
    # Operator tuning is persisted to /etc/ados/config.yaml so it survives a
    # service restart (the live apply is best-effort; persistence captures
    # intent, mirroring the tx-power route). Only valid values are persisted.
    persist: dict[str, Any] = {}

    # The encoder runs in the native video service, which takes its bitrate
    # from the attention profile and the adaptive controller, not from here.
    if body.bitrate_kbps is not None:
        warnings.append("bitrate_not_settable_on_this_surface")

    if body.fec_k is not None or body.fec_n is not None:
        cfg = app.config.video.wfb
        new_k = body.fec_k if body.fec_k is not None else cfg.fec_k
        new_n = body.fec_n if body.fec_n is not None else cfg.fec_n
        if await _radio_call("set_fec", warnings, new_k, new_n):
            # Persist only a valid ratio (n > k >= 1), the same invariant the
            # radio setter enforces, so a rejected ratio never reaches config.
            if new_k >= 1 and new_n > new_k:
                persist["fec_k"] = int(new_k)
                persist["fec_n"] = int(new_n)

    if body.mcs is not None:
        if await _radio_call("set_mcs", warnings, body.mcs):
            persist["mcs_index"] = int(body.mcs)

    if body.preset is not None:
        trio = await _apply_preset(body.preset, warnings)
        if trio is not None:
            # Persist the resolved trio too, so a runtime preset survives a
            # restart even for "conservative" (whose boot apply is a no-op).
            persist["wfb_link_preset"] = body.preset
            persist["mcs_index"], persist["fec_k"], persist["fec_n"] = trio

    if body.auto is not None or body.tier_idx is not None:
        if await _apply_tier(app, body, warnings):
            if body.tier_idx is not None:
                # A pinned rung implies adaptive off; persist the rung's FEC too.
                from ados.services.video.bitrate_controller import DEFAULT_TIERS

                rung = DEFAULT_TIERS[body.tier_idx]
                persist["adaptive_bitrate_enabled"] = False
                persist["fec_k"] = int(rung.fec_k)
                persist["fec_n"] = int(rung.fec_n)
            elif body.auto is not None:
                persist["adaptive_bitrate_enabled"] = bool(body.auto)

    if persist:
        from ados.api.routes.wfb import _persist_wfb_fields

        if not _persist_wfb_fields(persist):
            warnings.append("persist_failed")
        _mirror_wfb_config(app, persist)

    response = await get_video_config()
    response["warnings"] = warnings
    return response


async def _radio_call(op: str, warnings: list[str], *args: Any) -> bool:
    """Send one tuning op to the native radio's command socket.

    Returns True when the radio applied it. A refusal is named
    ``<op>_failed``; an absent socket (the radio is not running) is named
    ``radio_unavailable`` so the two are never confused.
    """
    from ados.services.wfb import cmd_client

    try:
        await getattr(cmd_client, op)(*args)
    except cmd_client.RadioCmdError:
        warnings.append(f"{op}_failed")
        return False
    except cmd_client.RadioCmdUnavailableError:
        warnings.append("radio_unavailable")
        return False
    return True


# Link preset -> (mcs, fec_k, fec_n). Mirrors the Rust link_preset_trio table
# (crates/ados-radio/src/config.rs); used to persist the resolved trio when
# the radio's echo omits it.
_PRESET_TRIOS: dict[str, tuple[int, int, int]] = {
    "conservative": (1, 8, 12),
    "balanced": (3, 8, 12),
    "aggressive": (5, 8, 10),
}


async def _apply_preset(preset: str, warnings: list[str]) -> tuple[int, int, int] | None:
    """Apply a named link preset through the radio, which resolves and pins
    the trio and echoes it back (the adaptive controller stays armed).
    Returns the applied trio, or None when the apply failed."""
    from ados.services.wfb import cmd_client

    trio = _PRESET_TRIOS.get(preset)
    if trio is None:
        warnings.append("set_preset_failed")
        return None
    try:
        resp = await cmd_client.set_preset(preset)
    except cmd_client.RadioCmdError:
        warnings.append("set_preset_failed")
        return None
    except cmd_client.RadioCmdUnavailableError:
        warnings.append("radio_unavailable")
        return None
    mcs, k, n = resp.get("mcs_index"), resp.get("fec_k"), resp.get("fec_n")
    if all(isinstance(v, int) for v in (mcs, k, n)):
        return (int(mcs), int(k), int(n))
    return trio


def _mirror_wfb_config(app: Any, updates: dict[str, Any]) -> None:
    """Mirror persisted wfb fields onto the live config object so an in-process
    reader sees the new values without a reload race (same pattern as the
    auto-pair toggle)."""
    cfg = getattr(app, "config", None)
    video = getattr(cfg, "video", None) if cfg is not None else None
    wfb_cfg = getattr(video, "wfb", None) if video is not None else None
    if wfb_cfg is None:
        return
    for key, value in updates.items():
        if hasattr(wfb_cfg, key):
            try:
                setattr(wfb_cfg, key, value)
            except Exception:  # noqa: BLE001 — assignment validation may reject
                pass


async def _apply_tier(app: Any, body: VideoConfigBody, warnings: list[str]) -> bool:
    """Apply the auto/manual link-tier toggle through the radio.

    ``auto`` arms the controller. A pinned ``tier_idx`` (or an explicit
    ``auto=False``) maps the default FEC ladder rung to a manual
    ``(mcs, fec_k, fec_n)`` trio: the rung sets the FEC and the configured MCS
    stands. ``auto=False`` with no rung holds the configured FEC/MCS so the
    controller stops stepping without forcing a different rung.
    """
    from ados.services.video.bitrate_controller import DEFAULT_TIERS

    cfg = app.config.video.wfb
    mcs = int(getattr(cfg, "mcs_index", 1) or 1)
    if body.tier_idx is not None:
        if not 0 <= body.tier_idx < len(DEFAULT_TIERS):
            warnings.append("set_manual_tier_failed")
            return False
        rung = DEFAULT_TIERS[body.tier_idx]
        return await _radio_call(
            "set_tier_manual", warnings, mcs, rung.fec_k, rung.fec_n
        )
    if body.auto is True:
        return await _radio_call("set_tier_auto", warnings)
    k = int(getattr(cfg, "fec_k", 8) or 8)
    n = int(getattr(cfg, "fec_n", 12) or 12)
    return await _radio_call("set_tier_manual", warnings, mcs, k, n)


__all__ = [
    "router",
    "_read_state_file",
    "_bitrate_controller_snapshot",
    "_hop_supervisor_snapshot",
    "_link_snapshot",
    "get_video_config",
    "set_video_config",
]
