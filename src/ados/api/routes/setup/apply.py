"""Batch-apply route + per-section dispatch + snapshot/rollback."""

from __future__ import annotations

from fastapi import APIRouter

from ados.api.deps import get_agent_app
from ados.setup import display_install
from ados.setup.advanced import apply_advanced
from ados.setup.models import SetupActionResult
from ados.setup.network import apply_network
from ados.setup.profile import apply_profile, apply_ui, apply_wfb
from ados.setup.service import apply_cloud_choice

from ._common import log
from ._models import ApplyRequest, ApplyResponse, ApplyResultSection
from ._restorers import (
    restore_advanced,
    restore_cloud,
    restore_network,
    restore_profile,
    restore_ui,
    restore_wfb,
)

router = APIRouter()


@router.post("/apply", response_model=ApplyResponse)
async def batch_apply_settings(request: ApplyRequest) -> ApplyResponse:
    """Apply a batch settings delta in one shot.

    Iterates the present sections in a fixed dependency order
    (profile, network, cloud, display, advanced), calls each per-
    section setter, and rolls back completed sections in reverse
    order if a later section fails. Returns a structured per-section
    result so the UI can show partial-success cleanly.
    """
    runtime = get_agent_app()

    sections: dict[str, ApplyResultSection] = {}
    completed: list[tuple[str, dict[str, object]]] = []
    rolled_back: list[str] = []

    order: list[tuple[str, object]] = [
        ("profile", request.profile),
        ("network", request.network),
        ("cloud", request.cloud),
        ("ui", request.ui),
        ("wfb", request.wfb),
        ("display", request.display),
        ("advanced", request.advanced),
    ]

    overall_ok = True
    for name, payload in order:
        if payload is None:
            continue
        snapshot = _capture_section_snapshot(runtime, name)
        try:
            result = await _apply_single_section(runtime, name, payload)
        except Exception as exc:  # noqa: BLE001 (never raise 500 from /apply)
            log.warning("apply_section_raised", section=name, error=str(exc))
            result = SetupActionResult(
                ok=False,
                message=f"Failed to apply {name}: {exc}",
            )
        section = ApplyResultSection(
            ok=bool(result.ok),
            message=str(result.message or ""),
            data=dict(result.data or {}),
        )
        sections[name] = section
        if section.ok:
            completed.append((name, snapshot))
        else:
            overall_ok = False
            rolled_back = _rollback_completed(runtime, completed)
            break

    return ApplyResponse(
        overall=overall_ok,
        sections=sections,
        rolled_back=rolled_back,
    )


async def _apply_single_section(
    runtime, name: str, payload
) -> SetupActionResult:
    """Dispatch one section to its setter."""
    if name == "profile":
        return apply_profile(
            runtime,
            profile=payload.profile,
            ground_role=payload.ground_role,
            auto_restart=payload.auto_restart,
        )
    if name == "network":
        return apply_network(runtime, payload)
    if name == "cloud":
        self_hosted = payload.self_hosted.model_dump() if payload.self_hosted else None
        return apply_cloud_choice(
            runtime,
            mode=payload.mode,
            self_hosted=self_hosted,
        )
    if name == "ui":
        return apply_ui(runtime, payload)
    if name == "wfb":
        return apply_wfb(runtime, payload)
    if name == "display":
        if not payload.display_id:
            return SetupActionResult(
                ok=False,
                message="display_id is required",
            )
        if payload.display_id == "none":
            try:
                display_install.write_skip_marker()
            except PermissionError as exc:
                return SetupActionResult(
                    ok=False,
                    message=(
                        "Cannot write display marker: "
                        f"{exc}"
                    ),
                )
            return SetupActionResult(
                ok=True,
                message="Display step skipped.",
                data={"display_id": "none"},
            )
        try:
            handle = await display_install.start_install(payload.display_id)
        except RuntimeError as exc:
            return SetupActionResult(ok=False, message=str(exc))
        except FileNotFoundError as exc:
            return SetupActionResult(ok=False, message=str(exc))
        return SetupActionResult(
            ok=True,
            message=f"Display install queued ({payload.display_id}).",
            data={"job_id": handle.job_id, "display_id": payload.display_id},
        )
    if name == "advanced":
        return apply_advanced(runtime, payload)
    return SetupActionResult(
        ok=False,
        message=f"Unknown section: {name}",
    )


def _capture_section_snapshot(runtime, name: str) -> dict[str, object]:
    """Best-effort snapshot of the live config slice a section touches.

    Used to revert that slice when a later section fails. Sections
    that have no clean undo (display install kicks off a subprocess)
    record an empty snapshot and are skipped on rollback.
    """
    config = getattr(runtime, "config", None)
    snap: dict[str, object] = {}
    if config is None:
        return snap
    try:
        if name == "profile":
            agent = getattr(config, "agent", None)
            ground = getattr(config, "ground_station", None)
            snap["profile"] = str(getattr(agent, "profile", "") or "")
            snap["ground_role"] = str(getattr(ground, "role", "") or "")
        elif name == "cloud":
            server = getattr(config, "server", None)
            snap["mode"] = str(getattr(server, "mode", "") or "")
            sh = getattr(server, "self_hosted", None)
            snap["self_hosted_url"] = str(getattr(sh, "url", "") or "")
            snap["self_hosted_mqtt_broker"] = str(
                getattr(sh, "mqtt_broker", "") or ""
            )
            snap["self_hosted_mqtt_port"] = int(
                getattr(sh, "mqtt_port", 0) or 0
            )
        elif name == "network":
            net = getattr(config, "network", None)
            wifi = getattr(net, "wifi_client", None)
            hotspot = getattr(net, "hotspot", None)
            snap["wifi_ssid"] = str(getattr(wifi, "ssid", "") or "")
            snap["wifi_password"] = str(getattr(wifi, "password", "") or "")
            snap["hotspot_enabled"] = bool(
                getattr(hotspot, "enabled", False)
            )
        elif name == "ui":
            ui = getattr(config, "ui", None)
            snap["theme"] = str(getattr(ui, "theme", "") or "")
        elif name == "wfb":
            video = getattr(config, "video", None)
            wfb = getattr(video, "wfb", None) if video is not None else None
            if wfb is not None:
                snap["channel"] = int(getattr(wfb, "channel", 0) or 0)
                snap["tx_power_dbm"] = int(getattr(wfb, "tx_power_dbm", 0) or 0)
                snap["mcs_index"] = int(getattr(wfb, "mcs_index", 0) or 0)
                snap["topology"] = str(getattr(wfb, "topology", "") or "")
        elif name == "advanced":
            agent = getattr(config, "agent", None)
            snap["log_level"] = str(getattr(agent, "log_level", "") or "")
    except Exception as exc:  # noqa: BLE001 (defensive)
        log.warning("snapshot_failed", section=name, error=str(exc))
    return snap


def _rollback_completed(
    runtime, completed: list[tuple[str, dict[str, object]]]
) -> list[str]:
    """Restore sections in reverse order. Returns the list of sections
    that were successfully reverted.

    Display installs cannot be undone trivially; the snapshot for
    display is empty and the section is skipped here. The returned
    list mirrors that behaviour.
    """
    reverted: list[str] = []
    for name, snap in reversed(completed):
        try:
            if name == "profile":
                restore_profile(runtime, snap)
            elif name == "cloud":
                restore_cloud(runtime, snap)
            elif name == "network":
                restore_network(runtime, snap)
            elif name == "ui":
                restore_ui(runtime, snap)
            elif name == "wfb":
                restore_wfb(runtime, snap)
            elif name == "advanced":
                restore_advanced(runtime, snap)
            else:
                continue
            reverted.append(name)
        except Exception as exc:  # noqa: BLE001 (best-effort rollback)
            log.warning("rollback_failed", section=name, error=str(exc))
    saver = getattr(getattr(runtime, "raw_runtime", None), "save_config", None)
    if reverted and callable(saver):
        try:
            saver()
        except Exception:
            pass
    return reverted
