"""WFB-ng link status and control routes."""

from __future__ import annotations

import asyncio
import json
import os
import subprocess
from pathlib import Path
from typing import Any

import yaml
from fastapi import APIRouter, HTTPException
from pydantic import BaseModel, Field

from ados.api.deps import get_agent_app
from ados.core.paths import CONFIG_YAML, WFB_FAILOVER_STATE_JSON
from ados.services.wfb.channel import STANDARD_CHANNELS, get_channel

# Local-bind to cloud-relay failover sidecar, written by the always-on
# auto-pair supervisor in a sibling process and read by
# GET /api/wfb/pair/failover-status below. The module-level alias keeps
# the path patchable in tests and decouples this route from the
# supervisor module's lifecycle.
FAILOVER_STATE_PATH = WFB_FAILOVER_STATE_JSON

# HTTP-level cap on the local-bind endpoint. The bind rendezvous itself
# is unbounded; this just prevents browsers and reverse proxies from
# cutting the request mid-flight. When the cap elapses the request
# fires a cancel and returns the terminal session snapshot.
_REST_LOCAL_BIND_TIMEOUT_S = 300.0  # 5 minutes

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


def _introspect_adapter(interface: str) -> dict[str, object]:
    """Direct driver / chipset / monitor-capability probe for an iface.

    Fallback for when the full adapter scan does not return an entry for
    the live interface (common once it is in monitor mode and the scan's
    name match misses). Reads the bound kernel driver from the sysfs
    ``device/driver`` symlink, derives a chipset label from the driver
    name, and checks monitor support from ``iw phy``. Best-effort: any
    field that cannot be determined comes back empty / False.
    """
    result: dict[str, object] = {
        "driver": "",
        "chipset": "",
        "supports_monitor": False,
    }
    if not interface:
        return result
    # Bound kernel driver via the sysfs symlink target.
    try:
        driver_link = os.readlink(f"/sys/class/net/{interface}/device/driver")
        driver = driver_link.rsplit("/", 1)[-1]
        result["driver"] = driver
        result["chipset"] = _chipset_from_driver(driver)
    except (FileNotFoundError, OSError):
        pass
    # Monitor-mode capability from iw phy. The phy index lives at
    # /sys/class/net/<iface>/phy80211/index; query that phy's modes.
    try:
        phy_index = Path(
            f"/sys/class/net/{interface}/phy80211/index"
        ).read_text().strip()
        proc = subprocess.run(
            ["iw", "phy", f"phy{phy_index}", "info"],
            capture_output=True,
            text=True,
            timeout=5,
        )
        if proc.returncode == 0 and "* monitor" in proc.stdout:
            result["supports_monitor"] = True
    except (FileNotFoundError, OSError, subprocess.TimeoutExpired):
        pass
    return result


def _chipset_from_driver(driver: str) -> str:
    """Map a bound kernel driver name to a human chipset label."""
    if not driver:
        return ""
    lowered = driver.lower()
    if "88x2eu" in lowered or "8812eu" in lowered:
        return "RTL8812EU"
    if "8812au" in lowered or "88xxau" in lowered:
        return "RTL8812AU"
    return driver


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


def _build_status_from_stats_file(wfb_cfg: object) -> dict:
    """Compose a /api/wfb response from /run/ados/wfb-stats.json.

    Used on the GS profile (and any other profile where ``app.wfb_manager()``
    is None) because the wfb stats live in a sibling process. The file
    is written ~once per second by whichever manager owns the radio;
    if it's stale (mtime > 10 s) we mark state="stale" so the LCD can
    render a degraded badge instead of presenting old data as live.

    Falls back to a zero-default block if the file doesn't exist yet
    (fresh boot, before the first PKT line has arrived).
    """
    import json as _json
    import time as _time

    from ados.core.paths import WFB_STATS_JSON

    # Static config defaults (used regardless of whether the file is
    # readable so the API response shape stays stable).
    cfg_channel = getattr(wfb_cfg, "channel", 0) if wfb_cfg is not None else 0
    cfg_tx_power = getattr(wfb_cfg, "tx_power_dbm", None) if wfb_cfg is not None else None
    cfg_tx_power_max = getattr(wfb_cfg, "tx_power_max_dbm", None) if wfb_cfg is not None else None
    cfg_topology = getattr(wfb_cfg, "topology", None) if wfb_cfg is not None else None
    cfg_mcs = getattr(wfb_cfg, "mcs_index", None) if wfb_cfg is not None else None

    base = {
        "state": "disabled",
        "interface": "",
        "channel": cfg_channel,
        "frequency_mhz": 0,
        "bandwidth_mhz": 0,
        "adapter": {
            "driver": "",
            "chipset": "",
            "supports_monitor": False,
        },
        "adapter_chipset": None,
        "adapter_injection_ok": False,
        "rssi_dbm": -100.0,
        "noise_dbm": -95.0,
        "snr_db": 0.0,
        "packets_received": 0,
        "packets_lost": 0,
        "loss_percent": 0.0,
        "fec_recovered": 0,
        "fec_failed": 0,
        "bitrate_kbps": 0,
        "rx_silent_seconds": None,
        "restart_count": 0,
        "samples": 0,
        "tx_power_dbm": cfg_tx_power,
        "tx_power_max_dbm": cfg_tx_power_max,
        "topology": cfg_topology,
        "mcs_index": cfg_mcs,
        "regulatory_domain": _read_regulatory_domain(),
    }

    try:
        st = WFB_STATS_JSON.stat()
        age_s = _time.time() - st.st_mtime
        with open(WFB_STATS_JSON) as f:
            payload = _json.load(f)
        if not isinstance(payload, dict):
            return base
        # Merge file payload over the defaults. The file already
        # contains the same key names as our response shape (the
        # writer in link_quality.persist_to_file builds it from
        # LinkStats.to_dict + the manager's `extra` dict).
        merged = dict(base)
        merged.update(payload)
        # Channel/topology/mcs from the file take precedence over the
        # static config (the live values reflect what the manager
        # actually applied via iw, which can differ from disk during
        # a transition).
        # Stale detection: anything older than 10 s is suspect.
        if age_s > 10.0:
            merged["state"] = "stale"
        merged["regulatory_domain"] = base["regulatory_domain"]
        # Re-derive frequency/bandwidth from the channel number using
        # the channel module's lookup so consumers get a consistent
        # shape regardless of who wrote the file.
        ch_info = get_channel(int(merged.get("channel") or 0))
        if ch_info:
            merged["frequency_mhz"] = ch_info.frequency_mhz
            merged["bandwidth_mhz"] = ch_info.bandwidth_mhz
        # Emit bitrate_mbps alongside the canonical bitrate_kbps so a
        # consumer that knows only the heartbeat-style key still gets
        # a populated value. Cheap forward-compat shim.
        bk = merged.get("bitrate_kbps")
        merged["bitrate_mbps"] = (
            round(float(bk) / 1000.0, 3)
            if isinstance(bk, (int, float)) and bk > 0
            else 0.0
        )
        return merged
    except (FileNotFoundError, ValueError, OSError):
        return base


def _native_radio_running() -> bool:
    """True when the native transmit plane (``ados-radio``) is the running
    radio implementation.

    Resolves the radio service's native-vs-packaged branch with the same
    rule the runtime-mode aggregate uses, so the knob routes to the command
    socket only when the native binary actually owns the radio (and the
    packaged Python manager is therefore absent). Total + cheap (it only
    stats files), safe to call on the request path.
    """
    from ados.core.runtime_mode import is_service_native

    return is_service_native("radio")


def _persist_wfb_fields(updates: dict[str, Any]) -> bool:
    """Atomically merge `updates` into the `video.wfb` block of the on-disk
    config so operator tuning survives a service restart.

    Mirrors the tmp-write + os.replace idiom used elsewhere in the agent.
    Returns True on success, False if the file is unreadable or unwritable.
    """
    if not updates:
        return True
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
        wfb_section.update(updates)
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


def _persist_tx_power(dbm: int) -> bool:
    """Atomically write `video.wfb.tx_power_dbm` to the on-disk config.

    Thin wrapper over `_persist_wfb_fields`, kept for the tx-power route.
    """
    return _persist_wfb_fields({"tx_power_dbm": int(dbm)})


@router.get("/wfb")
async def get_wfb_status():
    """Current WFB-ng link status: state, RSSI, channel, packet stats, adapter info."""
    app = get_agent_app()
    wfb = app.wfb_manager()
    cfg = app.config
    wfb_cfg = getattr(cfg.video, "wfb", None) if cfg is not None else None

    if wfb is None:
        # GS profile path: the WfbRxManager lives in ados-wfb-rx (a
        # different systemd unit than this api process), so we can't
        # call its stats() directly. The manager mirrors its current
        # snapshot to /run/ados/wfb-stats.json once per stats interval;
        # read that file as the canonical source. Falls through to a
        # zero-default block on a fresh boot before the file is
        # written, or after a long wfb_rx outage where mtime > 10 s.
        return _build_status_from_stats_file(wfb_cfg)
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
    interface_name = status.get("interface", "")
    try:
        from ados.services.wfb.adapter import detect_wfb_adapters

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

    # Fallback: the full scan can miss the live interface once it is in
    # monitor mode (the name match relies on `iw dev` enumeration that
    # behaves differently per driver). Probe the sysfs driver symlink +
    # iw phy directly so driver / chipset / supports_monitor are never
    # blank when an interface is up.
    if interface_name and not adapter_info.get("driver"):
        adapter_info = _introspect_adapter(interface_name)

    status["adapter"] = adapter_info

    # Forward-compat: emit bitrate_mbps alongside the canonical
    # bitrate_kbps so a consumer that knows only the heartbeat-style
    # key still gets a populated value.
    bk = status.get("bitrate_kbps")
    status["bitrate_mbps"] = (
        round(float(bk) / 1000.0, 3)
        if isinstance(bk, (int, float)) and bk > 0
        else 0.0
    )
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

    effective: int | None = None
    if _native_radio_running():
        # Native transmit plane: there is no in-process Python manager to
        # call, so forward the knob to the radio's command socket. An
        # unreachable socket falls through to the packaged-manager branch
        # (the native binary may not have come up yet on a fresh boot),
        # which then 503s if that manager is also absent.
        from ados.services.wfb import cmd_client

        try:
            effective = await cmd_client.set_tx_power(requested)
        except cmd_client.RadioCmdError as exc:
            raise HTTPException(
                status_code=500,
                detail={"error": "apply_failed", "message": str(exc)},
            ) from exc
        except cmd_client.RadioCmdUnavailableError:
            effective = _apply_tx_power_via_manager(app, requested)
    else:
        effective = _apply_tx_power_via_manager(app, requested)

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


def _apply_tx_power_via_manager(app, requested: int) -> int | None:
    """Apply the TX power through the in-process packaged WFB manager.

    The fallback path for when the native radio is not the running
    implementation (or its command socket is unreachable). Raises a 503
    when no manager is present and a 500 when the manager's apply fails,
    matching the prior behaviour of this route.
    """
    wfb = app.wfb_manager()
    if wfb is None:
        raise HTTPException(status_code=503, detail="WFB-ng service not running")
    apply = getattr(wfb, "apply_tx_power", None)
    if not callable(apply):
        return None
    try:
        return apply(requested)
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": "apply_failed", "message": str(exc)},
        ) from exc


# ---------------------------------------------------------------------
# Pair lifecycle: local-radio bind, status, unpair, auto-pair toggle.
# ---------------------------------------------------------------------


def _agent_role_from_profile(profile: str) -> str:
    """Map a resolved profile string to the bind-protocol role.

    The input must already be a resolved profile in either the wire
    form (``"drone"`` / ``"ground-station"``) or the underscore form
    (``"drone"`` / ``"ground_station"``). The raw ``config.agent.profile``
    value can be ``"auto"`` on a fresh install where the operator left
    the field at its default; do NOT pass that through here directly —
    use ``_current_role()`` instead, which routes through
    ``current_profile_and_role()`` so ``/etc/ados/profile.conf`` gets
    consulted when the config field is unresolved.
    """
    return "drone" if profile == "drone" else "gs"


def _current_role(app) -> str:
    """Resolve the agent's current bind-protocol role.

    Routes through ``ados.core.profile.current_profile_and_role`` so
    the on-disk ``/etc/ados/profile.conf`` is consulted whenever
    ``config.agent.profile`` is ``"auto"`` or empty. Without this hop
    a freshly-installed drone (where the default config carries
    ``profile: "auto"``) would be misclassified as a ground station,
    the auto-pair supervisor would run the GS bind-client flow, and
    the rendezvous would never converge.
    """
    from ados.core.profile import current_profile_and_role

    profile, _role = current_profile_and_role(app.config)
    # current_profile_and_role returns the hyphenated wire form
    # ("drone" / "ground-station"); _agent_role_from_profile only
    # checks ``== "drone"`` so the hyphen form maps correctly.
    return _agent_role_from_profile(profile)


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
    role = _current_role(app)

    from ados.services.ground_station.pair_manager import get_pair_manager

    pm = get_pair_manager()
    return await pm.status(role)


@router.post("/wfb/pair/local-bind")
async def post_wfb_pair_local_bind(request: LocalBindRequest) -> dict[str, Any]:
    """Open a local-radio bind window via the upstream wfb-ng protocol.

    Returns a session dict (`session_id`, `state`, `started_at`,
    `finished_at`, `error`, `fingerprint`, `peer_device_id`, `source`).
    The bind itself waits for a peer indefinitely, so this endpoint
    enforces an HTTP-level cap (5 minutes); when the cap elapses the
    request drives a cancel and returns whatever terminal state the
    session reached. Concurrent local-bind calls fail-fast with a 409.
    """
    app = get_agent_app()
    role = (
        request.role
        if request.role in ("drone", "gs")
        else _current_role(app)
    )

    from ados.services.wfb.bind_client import (
        BindBusyError,
        BindUnavailableError,
        forward_start_bind,
    )

    cancel_event = asyncio.Event()
    try:
        return await forward_start_bind(
            role=role,
            source="operator",
            peer_device_id=request.peer_device_id,
            cancel_event=cancel_event,
            timeout=_REST_LOCAL_BIND_TIMEOUT_S,
        )
    except BindBusyError as exc:
        raise HTTPException(
            status_code=409,
            detail={"error": {"code": "E_BIND_IN_PROGRESS", "message": str(exc)}},
        ) from exc
    except BindUnavailableError as exc:
        raise HTTPException(
            status_code=503,
            detail={"error": {"code": "E_BIND_UNAVAILABLE", "message": str(exc)}},
        ) from exc
    except TimeoutError as exc:
        # forward_start_bind returns the terminal session on its own
        # internal timeout, so this only fires if it surfaces a
        # TimeoutError to us. Preserve the original 504 envelope so the
        # GCS error handling stays valid.
        raise HTTPException(
            status_code=504,
            detail={
                "error": {
                    "code": "E_BIND_TIMEOUT",
                    "message": f"bind did not complete within "
                    f"{_REST_LOCAL_BIND_TIMEOUT_S}s",
                }
            },
        ) from exc


@router.get("/wfb/pair/local-bind")
async def get_wfb_pair_local_bind() -> dict[str, Any]:
    """Latest bind-session snapshot, or `{}` if none has run since boot."""
    from ados.services.wfb.bind_client import forward_status

    return await forward_status()


@router.post("/wfb/pair/unpair")
async def post_wfb_pair_unpair() -> dict[str, Any]:
    """Wipe both key files, clear pair state, restart the wfb service.

    Leaves `auto_pair_enabled = false` so the rig does not silently
    re-bind. The operator must explicitly re-arm to start auto-bind
    again.
    """
    app = get_agent_app()
    role = _current_role(app)

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
    role = _current_role(app)

    from ados.services.ground_station.pair_manager import get_pair_manager

    pm = get_pair_manager()
    result = await pm.set_auto_pair(bool(request.enabled), role)

    # Mirror onto the live config object so the auto_pair supervisor
    # observes the change without a config reload race.
    wfb_cfg = getattr(app.config.video, "wfb", None) if app.config is not None else None
    if wfb_cfg is not None:
        wfb_cfg.auto_pair_enabled = bool(result.get("auto_pair_enabled", False))

    return result


@router.get("/wfb/pair/failover-status")
async def get_failover_status() -> dict[str, str]:
    """Return the current local-bind to cloud-relay failover state.

    Reads the sidecar at ``/run/ados/wfb_failover.json`` written by the
    auto_pair supervisor in the ados-cloud process. Default is ``local``
    when the sidecar is missing or unreadable, which matches the
    supervisor's startup state.
    """
    path = Path(str(FAILOVER_STATE_PATH))
    if not path.exists():
        return {"failover_state": "local"}
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return {"failover_state": "local"}
    state = data.get("state", "local") if isinstance(data, dict) else "local"
    if state not in {"local", "cloud_relay", "failed"}:
        state = "local"
    return {"failover_state": state}
