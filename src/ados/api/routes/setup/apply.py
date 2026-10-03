"""Batch-apply route + per-section dispatch + snapshot/rollback."""

from __future__ import annotations

import asyncio

from fastapi import APIRouter

from ados.api.deps import get_agent_app
from ados.setup import display_install
from ados.setup.advanced import apply_advanced, read_board_override, write_board_override
from ados.setup.models import SetupActionResult
from ados.setup.network import apply_network
from ados.setup.profile import (
    apply_profile,
    apply_regulatory,
    apply_ui,
    apply_wfb,
    dispatch_profile_restart,
)
from ados.setup.service import apply_cloud_choice

from ._common import log
from ._models import ApplyRequest, ApplyResponse, ApplyResultSection

router = APIRouter()

# The config leaves each section writes. A rollback restores exactly these, as
# they read before the section ran, in one write.
_SECTION_LEAVES: dict[str, tuple[str, ...]] = {
    "profile": ("agent.profile", "ground_station.role"),
    "network": (
        "network.wifi_client.ssid",
        "network.wifi_client.password",
        "network.hotspot.enabled",
    ),
    "cloud": (
        "server.mode",
        "server.mqtt_password",
        "server.self_hosted.url",
        "server.self_hosted.mqtt_broker",
        "server.self_hosted.mqtt_port",
        "server.self_hosted.api_key",
        "pairing.convex_url",
    ),
    "ui": ("ui.theme",),
    "wfb": (
        "video.wfb.channel",
        "video.wfb.tx_power_dbm",
        "video.wfb.mcs_index",
        "video.wfb.topology",
    ),
    "regulatory": (
        "network.regulatory.mode",
        "network.regulatory.region",
        "network.regulatory.ack_operator",
        "network.regulatory.ack_at",
    ),
    "advanced": ("logging.level",),
}


@router.post("/apply", response_model=ApplyResponse)
async def batch_apply_settings(request: ApplyRequest) -> ApplyResponse:
    """Apply a batch settings delta in one shot.

    Iterates the present sections in a fixed dependency order, calls each
    per-section setter, and rolls back completed sections if a later section
    fails. Returns a structured per-section result so the UI can show
    partial-success cleanly.

    Every config-writing section runs before anything a rollback could not
    undo: the profile's supervisor restart is dispatched only after every
    section succeeded, and the display install (a root job) runs last. A
    rollback therefore restores the config and the board override and leaves
    nothing running that the restored config does not describe. The one
    exception is the self-hosted API key file, which is a secret the cloud
    section writes and a rollback does not rewrite.
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
        ("regulatory", request.regulatory),
        ("advanced", request.advanced),
        ("display", request.display),
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
            rolled_back = await asyncio.to_thread(_rollback_completed, runtime, completed)
            break

    profile_section = sections.get("profile")
    if (
        overall_ok
        and request.profile is not None
        and request.profile.auto_restart
        and profile_section is not None
        and profile_section.data.get("restart_required")
    ):
        profile_section.message += await asyncio.to_thread(
            dispatch_profile_restart, profile_section.data
        )

    return ApplyResponse(
        overall=overall_ok,
        sections=sections,
        rolled_back=rolled_back,
    )


async def _apply_single_section(
    runtime, name: str, payload
) -> SetupActionResult:
    """Dispatch one section to its setter.

    The setters shell out and take the config lock, so they run on a worker
    thread and the residual API keeps serving meanwhile.
    """
    if name == "profile":
        # The restart is dispatched by the batch only once every section
        # succeeded; a restart cannot be rolled back.
        return await asyncio.to_thread(
            lambda: apply_profile(
                runtime,
                profile=payload.profile,
                ground_role=payload.ground_role,
                auto_restart=False,
            )
        )
    if name == "network":
        return await asyncio.to_thread(apply_network, runtime, payload)
    if name == "cloud":
        self_hosted = payload.self_hosted.model_dump() if payload.self_hosted else None
        return await asyncio.to_thread(
            lambda: apply_cloud_choice(
                runtime,
                mode=payload.mode,
                self_hosted=self_hosted,
            )
        )
    if name == "ui":
        return await asyncio.to_thread(apply_ui, runtime, payload)
    if name == "wfb":
        return await asyncio.to_thread(apply_wfb, runtime, payload)
    if name == "regulatory":
        return await asyncio.to_thread(apply_regulatory, runtime, payload)
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
        return await asyncio.to_thread(apply_advanced, runtime, payload)
    return SetupActionResult(
        ok=False,
        message=f"Unknown section: {name}",
    )


def _read_leaf(config: object, dotted: str) -> object:
    value: object = config
    for part in dotted.split("."):
        value = getattr(value, part)
    return value


def _capture_section_snapshot(runtime, name: str) -> dict[str, object]:
    """The config leaves (and side files) a section writes, as they read now.

    Used to revert that section when a later section fails. The display
    section has no undo (it starts a root install job) and runs last, so it
    is never rolled back and records an empty snapshot.
    """
    snap: dict[str, object] = {}
    leaves = _SECTION_LEAVES.get(name, ())
    if leaves:
        config = runtime.config
        for dotted in leaves:
            try:
                snap[dotted] = _read_leaf(config, dotted)
            except AttributeError as exc:
                log.warning("snapshot_failed", section=name, leaf=dotted, error=str(exc))
    if name == "advanced":
        snap["__board_override__"] = read_board_override()
    return snap


def _rollback_completed(
    runtime, completed: list[tuple[str, dict[str, object]]]
) -> list[str]:
    """Restore the completed sections' config leaves in one write.

    Returns the sections that were reverted: all of them when the write
    landed, none when it failed (the failure is logged).
    """
    values: dict[str, object] = {}
    board_override: str | None = None
    names: list[str] = []
    for name, snap in completed:
        if name not in _SECTION_LEAVES:
            continue
        names.append(name)
        for dotted, value in snap.items():
            if dotted == "__board_override__":
                board_override = str(value)
            else:
                values[dotted] = value
    if not names:
        return []
    result = runtime.write_config(values)
    if not result:
        log.warning("rollback_failed", sections=names, error=result.error)
        return []
    if board_override is not None:
        try:
            write_board_override(board_override)
        except OSError as exc:
            log.warning("rollback_board_override_failed", error=str(exc))
            names.remove("advanced")
    return list(reversed(names))
