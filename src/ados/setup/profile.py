"""Profile selection helpers for the onboarding wizard.

The wizard's profile step lets the operator confirm or override the
auto-detected agent profile. Auto-detect lives in
``ados.bootstrap.profile_detect`` and is invoked here so the wizard's
``SetupStatus`` payload carries a fresh fingerprint without the webapp
having to call the probes itself.
"""

from __future__ import annotations

import socket
from typing import Any

from ados.setup.models import (
    ProfileSuggestion,
    SetupActionResult,
    UiApplyRequest,
    WfbApplyRequest,
)


def _hostname_suggested_profile() -> str | None:
    """Heuristic mapping from system hostname to expected profile.

    Returns one of ``"drone"`` or ``"ground_station"`` when the hostname
    matches a well-known prefix the bench uses, or ``None`` when the
    hostname carries no signal. The check is intentionally narrow so
    operators with custom hostnames see no advisory.
    """
    try:
        name = (socket.gethostname() or "").strip().lower()
    except OSError:
        return None
    if not name:
        return None
    if name.startswith(("groundnode", "groundstation", "gcs", "gs-")):
        return "ground_station"
    if name.startswith(("skynode", "drone", "rig-", "uav")):
        return "drone"
    return None


def build_profile_suggestion(config: Any) -> ProfileSuggestion:
    """Run the boot-time profile detector and shape the result for the wizard.

    ``config.agent.profile`` is passed through as ``confirmed=True`` only
    when the operator already set an explicit value (anything other than
    ``"auto"``). The detector itself short-circuits on a non-auto override
    so this is a single fast call in the explicit-profile case.
    """
    explicit = str(getattr(config.agent, "profile", "") or "")
    confirmed = explicit in ("drone", "ground_station")

    try:
        from ados.bootstrap.profile_detect import detect_profile

        # Pass None so the detector runs all probes even when config has
        # an explicit value: the wizard wants to show the live signals
        # alongside the operator's prior pick so the operator can spot
        # detect/config drift.
        result = detect_profile(config_override=None)
    except Exception:
        return ProfileSuggestion(
            detected="drone",
            source="default",
            confirmed=confirmed,
        )

    detected = str(result.get("profile") or "drone")
    if detected not in ("drone", "ground_station"):
        detected = "drone"

    source = str(result.get("source") or "detected")
    if source not in ("detected", "tiebreaker", "override", "default"):
        source = "detected"

    ground_role = str(getattr(config.ground_station, "role", "direct") or "direct")
    if ground_role not in ("direct", "relay", "receiver"):
        ground_role = "direct"

    return ProfileSuggestion(
        detected=detected,  # type: ignore[arg-type]
        source=source,  # type: ignore[arg-type]
        ground_role_hint=ground_role,  # type: ignore[arg-type]
        ground_score=int(result.get("ground_score", 0) or 0),
        air_score=int(result.get("air_score", 0) or 0),
        mesh_capable=bool(result.get("mesh_capable", False)),
        signals={
            str(name): bool(detected)
            for name, detected in (result.get("signals") or {}).items()
        },
        confirmed=confirmed,
        detected_at=result.get("detected_at"),
    )


def _restart_supervisor() -> tuple[bool, str]:
    """Restart `ados-supervisor` via systemd, with a subprocess fallback.

    Returns ``(ok, message)`` so callers can surface the failure mode
    cleanly. The D-Bus path is preferred when available; the subprocess
    fallback uses ``systemctl --no-block restart`` so the API process
    isn't itself killed before the response can be sent.
    """
    try:
        import dbus  # type: ignore

        bus = dbus.SystemBus()
        systemd = bus.get_object(
            "org.freedesktop.systemd1", "/org/freedesktop/systemd1"
        )
        manager = dbus.Interface(systemd, "org.freedesktop.systemd1.Manager")
        manager.RestartUnit("ados-supervisor.service", "replace")
        return True, "supervisor restart dispatched via systemd"
    except Exception:  # pragma: no cover - dbus unavailable on dev hosts
        pass

    try:
        import subprocess

        result = subprocess.run(
            ["systemctl", "--no-block", "restart", "ados-supervisor.service"],
            check=False,
            capture_output=True,
            text=True,
            timeout=5,
        )
        if result.returncode == 0:
            return True, "supervisor restart dispatched via systemctl"
        stderr = (result.stderr or "").strip()
        return False, f"systemctl restart failed: {stderr}"
    except Exception as exc:
        return False, f"supervisor restart unavailable: {exc}"


def apply_profile(
    runtime: Any,
    *,
    profile: str,
    ground_role: str | None = None,
    auto_restart: bool = False,
) -> SetupActionResult:
    """Persist the operator's profile choice to ``config.agent.profile``.

    When the operator picks ``ground_station``, ``ground_role`` selects
    the distributed-RX role on ``config.ground_station.role``. The role
    is ignored for the drone profile.

    On a profile change the per-profile completion markers (skipped
    steps that no longer apply) are intentionally left alone. The wizard
    re-derives every step's state from the live config, so a stale skip
    flag for the now-hidden step does no harm.

    When ``auto_restart`` is true and the profile actually changed,
    dispatch a supervisor restart so the new profile's services come up
    without the operator having to SSH in. The restart is non-blocking,
    so the route response lands before the agent goes down.
    """
    if profile not in ("drone", "ground_station"):
        return SetupActionResult(
            ok=False,
            message="profile must be 'drone' or 'ground_station'",
        )

    if profile == "ground_station":
        role = ground_role or "direct"
        if role not in ("direct", "relay", "receiver"):
            return SetupActionResult(
                ok=False,
                message="ground_role must be 'direct', 'relay', or 'receiver'",
            )
    else:
        role = None

    config = runtime.config
    previous_profile = str(getattr(config.agent, "profile", "") or "")
    previous_role = str(getattr(config.ground_station, "role", "") or "")

    config.agent.profile = profile
    if profile == "ground_station" and role is not None:
        config.ground_station.role = role  # type: ignore[assignment]

    saver = getattr(runtime.raw_runtime, "save_config", None)
    if callable(saver):
        try:
            saver()
        except Exception:
            pass

    changed = previous_profile != profile or (
        profile == "ground_station" and previous_role != role
    )
    data: dict[str, object] = {
        "profile": profile,
        "ground_role": role or "",
        "changed": changed,
    }
    if changed and previous_profile not in ("", "auto"):
        data["restart_required"] = True

    # Advisory: hostname carries a strong signal about expected
    # profile (e.g., `groundnode` should be a ground station). When
    # the chosen profile contradicts the hostname-derived expectation
    # the wizard still applies the choice — operators do reconfigure
    # — but surfaces an inline nudge so the swap is intentional. No
    # advisory when the hostname carries no signal.
    suggested = _hostname_suggested_profile()
    if suggested is not None and suggested != profile:
        try:
            hostname = socket.gethostname()
        except OSError:
            hostname = ""
        data["advisory"] = {
            "code": "profile_hostname_mismatch",
            "hostname": hostname,
            "hostname_suggests": suggested,
            "chosen": profile,
            "message": (
                f"Hostname '{hostname}' suggests profile '{suggested}', "
                f"but you picked '{profile}'. Confirm this is intentional."
            ),
        }

    if profile == "drone":
        message = "Profile set to drone."
    else:
        message = f"Profile set to ground station ({role})."

    if auto_restart and data.get("restart_required"):
        ok_restart, restart_msg = _restart_supervisor()
        data["auto_restart_attempted"] = True
        data["auto_restart_ok"] = ok_restart
        data["auto_restart_message"] = restart_msg
        if ok_restart:
            message += " Restarting agent."
        else:
            message += f" Restart failed: {restart_msg}."
    elif data.get("restart_required"):
        message += " Restart the agent to apply."

    return SetupActionResult(ok=True, message=message, data=data)


def apply_ui(
    runtime: Any,
    request: UiApplyRequest | None,
) -> SetupActionResult:
    """Persist a UI section update onto ``runtime.config.ui``.

    The LCD dashboards read the active palette through
    ``ados.services.ui.theme.current_palette()`` on every render tick,
    so a theme flip takes effect on the next paint without a service
    restart.

    Returns ``ok=True`` even when the request is empty so the batch
    apply route can iterate sections without special-casing absent
    payloads.
    """
    if request is None:
        return SetupActionResult(
            ok=True,
            message="No UI changes requested.",
            data={"changed": False},
        )

    config = runtime.config
    ui = getattr(config, "ui", None)
    if ui is None:
        return SetupActionResult(
            ok=False,
            message="UI configuration is not available on this agent.",
        )

    changed_fields: list[str] = []

    if request.theme is not None:
        new_theme = str(request.theme)
        if new_theme not in ("dark", "light"):
            return SetupActionResult(
                ok=False,
                message="theme must be 'dark' or 'light'.",
            )
        if str(ui.theme) != new_theme:
            ui.theme = new_theme  # type: ignore[assignment]
            changed_fields.append("theme")

    saver = getattr(getattr(runtime, "raw_runtime", None), "save_config", None)
    if changed_fields and callable(saver):
        try:
            saver()
        except Exception:
            pass

    data: dict[str, object] = {
        "changed": bool(changed_fields),
        "fields": changed_fields,
        "theme": str(ui.theme),
    }
    if changed_fields:
        message = f"UI updated ({', '.join(changed_fields)})."
    else:
        message = "No UI changes detected."
    return SetupActionResult(ok=True, message=message, data=data)


def apply_wfb(
    runtime: Any,
    request: WfbApplyRequest | None,
) -> SetupActionResult:
    """Persist a WFB radio config slice onto ``runtime.config.video.wfb``.

    ``channel`` and ``tx_power_dbm`` mirror the existing dedicated
    routes (``POST /api/wfb/channel``, ``PUT /api/wfb/tx-power``).
    ``mcs_index`` and ``topology`` have no dedicated route; this
    setter is the only path that updates them. Reboot-required flips
    are surfaced through ``data["restart_required"]`` so the caller
    can tally pending reboots.

    Returns ``ok=True`` on a no-op so the batch-apply route can
    iterate sections without special-casing absent payloads.
    """
    if request is None:
        return SetupActionResult(
            ok=True,
            message="No WFB changes requested.",
            data={"changed": False},
        )

    config = runtime.config
    video = getattr(config, "video", None)
    wfb = getattr(video, "wfb", None) if video is not None else None
    if wfb is None:
        return SetupActionResult(
            ok=False,
            message="WFB configuration is not available on this agent.",
        )

    changed_fields: list[str] = []
    restart_required = False

    if request.channel is not None:
        new_channel = int(request.channel)
        # Validate against the standard list to refuse a typo before it
        # reaches the wfb manager. Imported lazily so the setup module
        # does not pull the wfb service tree at import time.
        try:
            from ados.services.wfb.channel import STANDARD_CHANNELS, get_channel

            if get_channel(new_channel) is None:
                valid = [c.channel_number for c in STANDARD_CHANNELS]
                return SetupActionResult(
                    ok=False,
                    message=(
                        f"channel must be one of {valid}, got {new_channel}"
                    ),
                )
        except ImportError:
            pass
        if int(getattr(wfb, "channel", 0)) != new_channel:
            wfb.channel = new_channel
            changed_fields.append("channel")
            restart_required = True

    if request.tx_power_dbm is not None:
        requested = int(request.tx_power_dbm)
        ceiling = int(getattr(wfb, "tx_power_max_dbm", 15))
        if requested < 1:
            return SetupActionResult(
                ok=False,
                message=f"tx_power_dbm below floor (min 1), got {requested}",
            )
        if requested > ceiling:
            return SetupActionResult(
                ok=False,
                message=(
                    f"tx_power_dbm above ceiling (max {ceiling}), got {requested}"
                ),
            )
        if int(getattr(wfb, "tx_power_dbm", 0)) != requested:
            wfb.tx_power_dbm = requested
            changed_fields.append("tx_power_dbm")

    if request.mcs_index is not None:
        new_mcs = int(request.mcs_index)
        # MCS index range is 0..7 for the modulation table the wfb
        # transport understands. Refuse anything outside that.
        if new_mcs < 0 or new_mcs > 7:
            return SetupActionResult(
                ok=False,
                message=f"mcs_index must be 0..7, got {new_mcs}",
            )
        if int(getattr(wfb, "mcs_index", 0)) != new_mcs:
            wfb.mcs_index = new_mcs
            changed_fields.append("mcs_index")
            restart_required = True

    if request.topology is not None:
        new_topo = str(request.topology)
        if new_topo not in ("host_vbus", "powered_hub", "external_5v"):
            return SetupActionResult(
                ok=False,
                message=(
                    "topology must be one of host_vbus / powered_hub / "
                    f"external_5v, got {new_topo}"
                ),
            )
        if str(getattr(wfb, "topology", "")) != new_topo:
            wfb.topology = new_topo  # type: ignore[assignment]
            changed_fields.append("topology")
            restart_required = True

    saver = getattr(getattr(runtime, "raw_runtime", None), "save_config", None)
    if changed_fields and callable(saver):
        try:
            saver()
        except Exception:
            pass

    data: dict[str, object] = {
        "changed": bool(changed_fields),
        "fields": changed_fields,
    }
    if restart_required:
        data["restart_required"] = True
    if changed_fields:
        message = f"WFB updated ({', '.join(changed_fields)})."
    else:
        message = "No WFB changes detected."
    return SetupActionResult(ok=True, message=message, data=data)
