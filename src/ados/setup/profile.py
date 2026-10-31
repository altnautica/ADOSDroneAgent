"""Profile selection helpers for the onboarding wizard.

The wizard's profile step lets the operator confirm or override the
auto-detected agent profile. Auto-detect lives in
``ados.bootstrap.profile_detect``; its stored result is shaped here so the
wizard's ``SetupStatus`` payload carries the fingerprint without re-running
the probes on every poll.
"""

from __future__ import annotations

import socket
from pathlib import Path
from typing import Any

import httpx

from ados.core._contracts_generated import PROFILES
from ados.core.paths import ADOS_RUN_DIR
from ados.setup._persist import persist_config
from ados.setup.models import (
    ProfileSuggestion,
    SetupActionResult,
    UiApplyRequest,
    WfbApplyRequest,
)

#: The native control surface's trusted on-box socket.
CONTROL_SOCK = ADOS_RUN_DIR / "control.sock"
_NATIVE_TIMEOUT_S = 10.0

#: Profiles the wizard can pick (and the detector can suggest).
WIZARD_PROFILES = ("drone", "ground_station", "visor")


def _hostname_suggested_profile() -> str | None:
    """Heuristic mapping from system hostname to expected profile.

    Returns one of ``"drone"``, ``"ground_station"`` or ``"visor"`` when the hostname
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
    if name.startswith(("groundstation", "gcs", "gs-")):
        return "ground_station"
    if name.startswith(("skynode", "drone", "rig-", "uav")):
        return "drone"
    if name.startswith("visor"):
        return "visor"
    return None


def build_profile_suggestion(
    config: Any, snapshot_path: Path | None = None
) -> ProfileSuggestion:
    """Shape the stored profile detection for the wizard.

    The probes scan I2C, open UARTs and read the state socket, so a status
    poll never runs them: it reads the snapshot the boot detection wrote
    (``/run/ados/profile-detect.json``). When no snapshot exists yet the
    detection runs once and stores one. ``POST /api/v1/setup/profile/redetect``
    refreshes it on demand.

    ``config.agent.profile`` is passed through as ``confirmed=True`` only
    when the operator already set an explicit value (anything other than
    ``"auto"``).
    """
    try:
        from ados.bootstrap import profile_detect

        path = snapshot_path or profile_detect.PROFILE_DETECT_JSON
        result = profile_detect.read_detect_snapshot(path)
        if result is None:
            result = profile_detect.redetect(path)
    except Exception:
        result = None
    return _suggestion_from(config, result)


def redetect_profile_suggestion(
    config: Any, snapshot_path: Path | None = None
) -> ProfileSuggestion:
    """Run the probes now, replace the stored snapshot, and shape the result."""
    from ados.bootstrap import profile_detect

    result = profile_detect.redetect(snapshot_path or profile_detect.PROFILE_DETECT_JSON)
    return _suggestion_from(config, result)


def _suggestion_from(config: Any, result: dict[str, Any] | None) -> ProfileSuggestion:
    explicit = str(getattr(config.agent, "profile", "") or "")
    confirmed = explicit in PROFILES
    if result is None:
        return ProfileSuggestion(detected="drone", source="default", confirmed=confirmed)

    detected = str(result.get("profile") or "drone")
    if detected not in WIZARD_PROFILES:
        detected = "drone"

    source = str(result.get("source") or "detected")
    if source not in ("detected", "tiebreaker", "override", "default"):
        source = "detected"

    # Read-only: the suggestion never writes profile.conf. Recording a
    # detected profile is the boot seed's job.
    ground_role = str(getattr(config.ground_station, "role", "direct") or "direct")
    if ground_role not in ("direct", "relay", "receiver"):
        ground_role = "direct"

    return ProfileSuggestion(
        detected=detected,  # type: ignore[arg-type]
        source=source,  # type: ignore[arg-type]
        ground_role_hint=ground_role,  # type: ignore[arg-type]
        ground_score=int(result.get("ground_score", 0) or 0),
        air_score=int(result.get("air_score", 0) or 0),
        visor_score=int(result.get("visor_score", 0) or 0),
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


def _effective_profile(config: Any) -> str:
    """The profile the node is running now, underscore form.

    ``agent.profile: auto`` resolves through profile.conf exactly as the
    services do at start, so a change away from an auto-detected profile is
    recognised as a change that needs a restart.
    """
    from ados.core.profile import current_profile_and_role

    profile, _role = current_profile_and_role(config)
    return profile.replace("-", "_")


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

    ``restart_required`` compares against the profile the node is running
    (``auto`` resolved), not the literal config value. When ``auto_restart``
    is true and a restart is required, dispatch a supervisor restart so the
    new profile's services come up without the operator having to SSH in.
    The restart is non-blocking, so the route response lands before the
    agent goes down.
    """
    if profile not in WIZARD_PROFILES:
        return SetupActionResult(
            ok=False,
            message="profile must be 'drone', 'ground_station' or 'visor'",
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
    running_profile = _effective_profile(config)

    values: dict[str, object] = {}
    if previous_profile != profile:
        values["agent.profile"] = profile
    if profile == "ground_station" and previous_role != role:
        values["ground_station.role"] = role
    failed = persist_config(runtime, values, what="Profile")
    if failed is not None:
        return failed

    changed = bool(values)
    data: dict[str, object] = {
        "profile": profile,
        "ground_role": role or "",
        "changed": changed,
    }
    if running_profile != profile or (
        profile == "ground_station" and previous_role != role
    ):
        data["restart_required"] = True

    # Advisory: hostname carries a strong signal about expected
    # profile (e.g., `gs-example` should be a ground station). When
    # the chosen profile contradicts the hostname-derived expectation
    # the wizard still applies the choice — operators do reconfigure
    # — but surfaces an inline nudge so the swap is intentional. No
    # advisory when the hostname carries no signal.
    #
    # Fire only on the FIRST explicit choice (previous_profile is
    # empty or "auto"). Once the operator has set the profile to a
    # concrete value, subsequent re-applies are deliberate and the
    # advisory becomes noise — re-running install.sh --upgrade
    # would otherwise re-show the nudge every time on bench rigs
    # whose hostname/profile pairing is intentional.
    is_first_explicit_choice = previous_profile in ("", "auto")
    suggested = _hostname_suggested_profile()
    if is_first_explicit_choice and suggested is not None and suggested != profile:
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

    if profile == "ground_station":
        message = f"Profile set to ground station ({role})."
    else:
        message = f"Profile set to {profile}."

    if auto_restart and data.get("restart_required"):
        message += dispatch_profile_restart(data)
    elif data.get("restart_required"):
        message += " Restart the agent to apply."

    return SetupActionResult(ok=True, message=message, data=data)


def dispatch_profile_restart(data: dict[str, object]) -> str:
    """Restart the supervisor for a profile change; record the outcome in ``data``.

    Returns the sentence to append to the operator-facing message.
    """
    ok_restart, restart_msg = _restart_supervisor()
    data["auto_restart_attempted"] = True
    data["auto_restart_ok"] = ok_restart
    data["auto_restart_message"] = restart_msg
    if ok_restart:
        return " Restarting agent."
    return f" Restart failed: {restart_msg}."


def apply_ui(
    runtime: Any,
    request: UiApplyRequest | None,
) -> SetupActionResult:
    """Persist a UI section update onto ``runtime.config.ui``.

    The native display service reads the active palette on every render
    tick, so a theme flip takes effect on the next paint without a
    service restart.

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

    values: dict[str, object] = {}
    theme = str(ui.theme)

    if request.theme is not None:
        new_theme = str(request.theme)
        if new_theme not in ("dark", "light"):
            return SetupActionResult(
                ok=False,
                message="theme must be 'dark' or 'light'.",
            )
        if theme != new_theme:
            values["ui.theme"] = new_theme
            theme = new_theme

    failed = persist_config(runtime, values, what="UI settings")
    if failed is not None:
        return failed

    changed_fields = [path.rsplit(".", 1)[-1] for path in values]
    data: dict[str, object] = {
        "changed": bool(changed_fields),
        "fields": changed_fields,
        "theme": theme,
    }
    if changed_fields:
        message = f"UI updated ({', '.join(changed_fields)})."
    else:
        message = "No UI changes detected."
    return SetupActionResult(ok=True, message=message, data=data)


def _native_call(
    client: httpx.Client, method: str, path: str, body: dict[str, object]
) -> tuple[int, object]:
    """One request to the native control surface: ``(status, decoded body)``."""
    try:
        resp = client.request(method, path, json=body)
    except httpx.HTTPError as exc:
        return 503, {"detail": f"control surface unreachable: {exc}"}
    try:
        return resp.status_code, resp.json()
    except ValueError:
        return resp.status_code, resp.text


def _control_transport() -> httpx.BaseTransport:
    """The transport to the native control surface's on-box socket."""
    return httpx.HTTPTransport(uds=str(CONTROL_SOCK))


def apply_wfb(
    runtime: Any,
    request: WfbApplyRequest | None,
    *,
    force: bool = False,
) -> SetupActionResult:
    """Apply a WFB radio slice.

    ``channel`` and ``tx_power_dbm`` go through the native routes over the
    control socket (``POST /api/wfb/channel``, ``PUT /api/wfb/tx-power``), so
    the radio is retuned live and the native validation, armed interlock and
    persistence apply; their answers are returned under ``data["native"]``.
    ``force`` is the caller's armed override, already checked by the caller.
    ``mcs_index`` and ``topology`` have no native route and are written to
    config; they take effect on a restart (``data["restart_required"]``).
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

    # Validate the config-only leaves before any live change.
    values: dict[str, object] = {}
    if request.mcs_index is not None:
        new_mcs = int(request.mcs_index)
        if new_mcs < 0 or new_mcs > 7:
            return SetupActionResult(ok=False, message=f"mcs_index must be 0..7, got {new_mcs}")
        if int(getattr(wfb, "mcs_index", 0)) != new_mcs:
            values["video.wfb.mcs_index"] = new_mcs
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
            values["video.wfb.topology"] = new_topo

    calls: list[tuple[str, str, str, dict[str, object], int]] = []
    if request.channel is not None and int(getattr(wfb, "channel", 0)) != int(request.channel):
        body: dict[str, object] = {"channel": int(request.channel), "force": force}
        calls.append(("channel", "POST", "/api/wfb/channel", body, 202))
    if request.tx_power_dbm is not None and int(getattr(wfb, "tx_power_dbm", 0)) != int(
        request.tx_power_dbm
    ):
        body = {"tx_power_dbm": int(request.tx_power_dbm), "force": force}
        calls.append(("tx_power_dbm", "PUT", "/api/wfb/tx-power", body, 200))

    native: dict[str, object] = {}
    changed_fields: list[str] = []
    if calls:
        with httpx.Client(
            transport=_control_transport(),
            base_url="http://ados-control",
            timeout=_NATIVE_TIMEOUT_S,
        ) as client:
            for field_name, method, path, body, expected in calls:
                status, answer = _native_call(client, method, path, body)
                native[field_name] = {"status": status, "body": answer}
                if status != expected:
                    return SetupActionResult(
                        ok=False,
                        message=f"WFB {field_name} refused ({status}): {answer}",
                        data={"native": native, "fields": changed_fields},
                    )
                changed_fields.append(field_name)

    failed = persist_config(runtime, values, what="WFB settings")
    if failed is not None:
        return failed
    changed_fields += [path.rsplit(".", 1)[-1] for path in values]

    data: dict[str, object] = {"changed": bool(changed_fields), "fields": changed_fields}
    if native:
        data["native"] = native
    if values:
        data["restart_required"] = True
    if changed_fields:
        message = f"WFB updated ({', '.join(changed_fields)})."
    else:
        message = "No WFB changes detected."
    return SetupActionResult(ok=True, message=message, data=data)
