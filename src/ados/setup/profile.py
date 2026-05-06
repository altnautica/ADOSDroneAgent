"""Profile selection helpers for the onboarding wizard.

The wizard's profile step lets the operator confirm or override the
auto-detected agent profile. Auto-detect lives in
``ados.bootstrap.profile_detect`` and is invoked here so the wizard's
``SetupStatus`` payload carries a fresh fingerprint without the webapp
having to call the probes itself.
"""

from __future__ import annotations

from typing import Any

from ados.setup.models import ProfileSuggestion, SetupActionResult


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


def apply_profile(
    runtime: Any,
    *,
    profile: str,
    ground_role: str | None = None,
) -> SetupActionResult:
    """Persist the operator's profile choice to ``config.agent.profile``.

    When the operator picks ``ground_station``, ``ground_role`` selects
    the distributed-RX role on ``config.ground_station.role``. The role
    is ignored for the drone profile.

    On a profile change the per-profile completion markers (skipped
    steps that no longer apply) are intentionally left alone. The wizard
    re-derives every step's state from the live config, so a stale skip
    flag for the now-hidden step does no harm.
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

    if profile == "drone":
        message = "Profile set to drone."
    else:
        message = f"Profile set to ground station ({role})."
    if data.get("restart_required"):
        message += " Restart the agent to apply."

    return SetupActionResult(ok=True, message=message, data=data)
