"""Wizard step assembly for the universal setup contract.

This module owns the pure, kwarg-driven construction of the step list
that the operator walks through during onboarding. Inputs are the
already-collected status snapshots (mavlink / video / network / remote
access / cloud choice / profile suggestion / hardware check) plus the
operator-facing Mission Control URL. Outputs are a list of
``SetupStep`` instances in spec-39 order.

The function is intentionally pure so it can be golden-output tested
without a running agent. It lives in its own module so the larger
``ados.setup.service`` surface (which deals with sockets, host-header
validation, and access URL assembly) does not accumulate the wizard
tree as a sidecar concern.

The two entry points are imported back into ``ados.setup.service``
verbatim for backwards compatibility with callers that already import
``_resolve_display_step`` and ``_setup_steps`` from that module.
"""

from __future__ import annotations

from ados.setup.hardware_check import derive_step_state
from ados.setup.models import (
    CloudChoiceStatus,
    HardwareCheckStatus,
    MavlinkAccess,
    NetworkStatus,
    ProfileSuggestion,
    RemoteAccessStatus,
    SetupStep,
    VideoAccess,
)


def _resolve_display_step(
    hardware_check: HardwareCheckStatus,
) -> tuple[str, str]:
    """Map the hardware-check ``display`` row onto wizard step state + detail.

    Returns ``(state, detail)`` where ``state`` is one of the
    ``SetupStepState`` literal values. The wizard step is the action
    surface; the table row in the hardware-check step provides the same
    information as a diagnostic readout, so both stay in sync because
    they read from the same probe.
    """
    item = next(
        (i for i in hardware_check.items if i.id == "display"),
        None,
    )
    if item is None:
        return "needs_action", "Plug a supported SPI LCD to configure local display."
    detail = item.detail or item.label
    # Probe states defined in `_check_display`:
    #   - ok: configured + bound + driver matches
    #   - warning: configured but fb1 absent OR driver mismatch
    #   - unknown: no display.conf at all (or display_id=none after skip)
    if item.state == "ok":
        return "complete", detail
    if item.state == "unknown":
        # Check whether the operator explicitly skipped (display_id=none).
        # The hardware-check probe reports state="unknown" both for
        # "never configured" and for the explicit-skip path; the wizard
        # distinguishes via /etc/ados/display.conf.
        from ados.core.paths import DISPLAY_CONF_PATH

        if DISPLAY_CONF_PATH.exists():
            try:
                text = DISPLAY_CONF_PATH.read_text()
            except OSError:
                text = ""
            if "display_id=none" in text:
                return "optional", "Display step skipped"
        return "needs_action", detail or "No local display configured"
    if item.state == "warning":
        return "needs_action", detail
    return "needs_action", detail or "Configure local display"


def _setup_steps(
    *,
    profile: str,
    mavlink: MavlinkAccess,
    video: VideoAccess,
    network: NetworkStatus,
    remote: RemoteAccessStatus,
    cloud_choice: CloudChoiceStatus,
    profile_suggestion: ProfileSuggestion,
    hardware_check: HardwareCheckStatus,
    mission_control_url: str,
    regulatory_mode: str = "unrestricted",
    regulatory_region: str | None = None,
) -> list[SetupStep]:
    """Emit the canonical onboarding steps in spec-39 order.

    Profile branches drop steps that do not apply: the drone profile has
    no ``ground_receiver`` step; the ground profile has no ``mavlink``
    step. The ``profile`` step is profile-agnostic and lets the operator
    confirm or override the auto-detected fingerprint. The
    ``hardware_check`` step renders a per-component readout for the
    chosen profile.
    """
    is_drone = profile != "ground_station"
    is_ground = profile == "ground_station"
    network_complete = bool(network.local_ips) or bool(network.hotspot_enabled)
    cloud_paired = cloud_choice.paired
    cloud_local = cloud_choice.mode == "local"
    profile_confirmed = profile_suggestion.confirmed and profile in (
        "drone",
        "ground_station",
    )
    hw_state, hw_detail = derive_step_state(hardware_check)

    steps: list[SetupStep] = []

    steps.append(
        SetupStep(
            id="welcome",
            label="Welcome",
            state="complete",
            detail="Device identity available",
        )
    )

    steps.append(
        SetupStep(
            id="profile",
            label="Profile",
            state="complete" if profile_confirmed else "needs_action",
            detail=(
                f"Confirmed as {profile}"
                if profile_confirmed
                else "Confirm or change the profile for this device"
            ),
            action_label="Choose profile",
            href="/setup?step=profile",
        )
    )

    # Operating-region step. Always optional so it never blocks the
    # wizard's finish: the default posture is unrestricted, which is a
    # valid state with no operator action. An operator who wants their
    # jurisdiction's channel/power limits enforced pins a region here.
    pinned_region = (regulatory_region or "").strip().upper()
    region_pinned = regulatory_mode == "region" and bool(pinned_region)
    steps.append(
        SetupStep(
            id="region",
            label="Operating region",
            state="complete" if region_pinned else "optional",
            detail=(
                f"Pinned to {pinned_region}"
                if region_pinned
                else "Unrestricted (default). You are responsible for local RF compliance."
            ),
            action_label="Set operating region",
            href="/setup?step=region",
        )
    )

    # Network readout used to be its own step. The wizard's welcome step
    # now surfaces the same data inline as a chip row, so we no longer
    # render a dedicated network step. The /network.html surface stays as
    # the standalone diagnostic page; only the wizard step is dropped.
    # The network_complete signal is still computed above and consumed by
    # the welcome step state below.

    # Welcome state is upgraded to needs_action when there is no usable
    # local network so the operator does not coast past the chip row.
    if not network_complete and steps:
        steps[0] = SetupStep(
            id="welcome",
            label="Welcome",
            state="needs_action",
            detail="Bring up Wi-Fi, hotspot, USB tether, or LAN to continue.",
        )

    steps.append(
        SetupStep(
            id="hardware_check",
            label="Hardware check",
            state=hw_state,  # type: ignore[arg-type]
            detail=hw_detail,
            action_label="Open hardware check",
            href="/setup?step=hardware_check",
        )
    )

    steps.append(
        SetupStep(
            id="cloud_choice",
            label="Cloud posture",
            state=(
                "complete"
                if cloud_choice.mode in ("cloud", "self_hosted", "local")
                and (cloud_local or cloud_choice.backend_url)
                else "needs_action"
            ),
            detail=(
                "Local-only mode. No cloud relay configured."
                if cloud_local
                else f"Connected to {cloud_choice.backend_url}"
                if cloud_choice.backend_url
                else "Choose a cloud posture for this device"
            ),
            action_label="Choose cloud posture",
            href="/setup?step=cloud_choice",
        )
    )

    # Pair step is only meaningful when the device is set up to talk to a
    # cloud or self-hosted backend. Local-only deployments hide it entirely
    # so the wizard does not waste an operator's attention on a step they
    # have nothing to do on.
    if not cloud_local:
        steps.append(
            SetupStep(
                id="pair",
                label="Pair with Mission Control",
                state="complete" if cloud_paired else "needs_action",
                detail=(
                    "Device is paired."
                    if cloud_paired
                    else "Show this device's code or accept one from Mission Control."
                ),
                action_label="Pair this device",
                href="/setup?step=pair",
            )
        )

    if is_drone:
        # When the FC is not connected, surface WHY from the hardware-check FC
        # item (no serial device / serial present but silent) plus the fix,
        # instead of the generic "connect or configure" line.
        fc_item = next((it for it in hardware_check.items if it.id == "fc"), None)
        # The FC step is complete when the flight controller is REACHABLE — a
        # live MAVLink link OR a detected MSP FC (Betaflight/iNav), which the
        # hardware check reports as state="ok" even though it emits no MAVLink
        # heartbeat. Gating on mavlink.connected alone left an MSP rig stuck at
        # "needs action" forever.
        fc_ok = fc_item is not None and fc_item.state == "ok"
        if mavlink.connected:
            fc_detail = "MAVLink telemetry is live."
        elif fc_ok:
            fc_detail = fc_item.detail or "Flight controller detected."
        elif fc_item is not None and fc_item.detail:
            fc_detail = fc_item.detail
            if fc_item.fix_hint:
                fc_detail = f"{fc_detail} {fc_item.fix_hint}"
        else:
            fc_detail = (
                "No flight controller detected on USB/serial. "
                "Connect the FC over USB and re-check."
            )
        steps.append(
            SetupStep(
                id="mavlink",
                label="Flight controller",
                state="complete" if (mavlink.connected or fc_ok) else "needs_action",
                detail=fc_detail,
                action_label="Open MAVLink",
                href="/mavlink.html",
            )
        )

    # Sharper detail string when a camera is detected by the HAL but the
    # pipeline has not yet reached running state. Helps the operator see
    # that the agent IS aware of their hardware so they don't think the
    # camera is dead.
    camera_detected = any(
        item.id == "camera" and item.state == "ok"
        for item in hardware_check.items
    )
    if video.state == "running":
        video_detail = "WHEP video is live"
    elif camera_detected:
        video_detail = "Camera detected. Click Start video to begin streaming."
    else:
        video_detail = "No camera or receiver detected. Skip if you do not need video on this device."

    steps.append(
        SetupStep(
            id="video",
            label="Video",
            state="complete" if video.state == "running" else "needs_action",
            detail=video_detail,
            action_label="Open Video",
            href="/video.html",
        )
    )

    if is_ground:
        steps.append(
            SetupStep(
                id="ground_receiver",
                label="Ground receiver",
                state="complete" if video.state == "running" else "needs_action",
                detail=(
                    "Receiving WFB video."
                    if video.state == "running"
                    else "Configure the WFB receiver and mesh role, then bind to a drone."
                ),
                action_label="Open Ground station",
                href="/ground.html",
            )
        )
        # Local-display step. Surfaces an SPI LCD attached over the
        # 40-pin expansion header (Waveshare 3.5" RPi LCD on Cubie A7Z
        # or Rock 5C ground-station builds). State is derived from the
        # same hardware-check item that the diagnostic table uses, so
        # the two surfaces stay in sync. Hidden on drone profile
        # because no LCD path exists on air-side rigs in this revision.
        display_state, display_detail = _resolve_display_step(hardware_check)
        steps.append(
            SetupStep(
                id="display",
                label="Local display",
                state=display_state,  # type: ignore[arg-type]
                detail=display_detail,
                action_label="Configure display",
                href="/setup?step=display",
            )
        )

    steps.append(
        SetupStep(
            id="remote_access",
            label="Remote access",
            state="complete" if remote.status == "running" else "optional",
            detail=(
                "Cloudflare tunnel is running."
                if remote.status == "running"
                else "Optional — reach this device beyond the LAN via a Cloudflare tunnel."
            ),
            action_label="Open Remote access",
            href="/remote.html",
        )
    )

    steps.append(
        SetupStep(
            id="finish",
            label="Finish",
            state="complete" if mavlink.connected or video.state == "running" else "optional",
            detail=(
                "Open Mission Control when local telemetry or video is ready"
                if mission_control_url
                else "Open Mission Control on your computer once telemetry or video is ready"
            ),
            action_label="Open Mission Control" if mission_control_url else "",
            href=mission_control_url,
        )
    )

    return steps


# Public alias; existing call sites use the underscore-prefixed name as
# a module-private helper, but the friendlier name is offered here for
# new callers and tests that prefer to point at the canonical entry
# point.
build_setup_steps = _setup_steps
