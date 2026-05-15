"""Profile and role helpers for the wire contract.

The agent stores ``profile`` as ``"drone"`` or ``"ground_station"``
internally (underscore form, set by ``bootstrap/profile_detect.py`` and
read from ``/etc/ados/profile.conf``). The GCS-facing wire contract uses
the hyphenated form ``"ground-station"``. This module is the single
place the two forms are bridged so the heartbeat, the pairing-info
endpoint, and the mDNS TXT records all emit the same shape.
"""

from __future__ import annotations

from ados.core.paths import PROFILE_CONF


def normalize_profile(raw: str | None) -> str:
    """Return the wire-contract profile string."""
    if raw == "ground_station":
        return "ground-station"
    # "drone", "auto", "", None → "drone" for wire purposes.
    return "drone"


def _read_profile_conf_value() -> str | None:
    """Read the canonical ``profile:`` value out of /etc/ados/profile.conf.

    install.sh and the wizard both persist the operator-facing profile
    to this file; the bootstrap detector also writes here when run with
    ``write_profile_conf``. Returns ``None`` on any error or unrecognized
    value so the caller falls back to the safe default.
    """
    try:
        if not PROFILE_CONF.exists():
            return None
        # Try YAML form first ("profile: X"). Fall back to legacy
        # key=value ("profile=X") so older installs don't silently
        # downgrade to "drone" when this code is the first reader.
        for line in PROFILE_CONF.read_text(encoding="utf-8").splitlines():
            stripped = line.strip()
            if not stripped or stripped.startswith("#"):
                continue
            if stripped.startswith("profile:"):
                value = stripped.split(":", 1)[1].strip().strip('"\'')
                if value in ("drone", "ground_station", "ground-station"):
                    return value.replace("-", "_")
            elif stripped.startswith("profile="):
                value = stripped.split("=", 1)[1].strip().strip('"\'')
                if value in ("drone", "ground_station", "ground-station"):
                    return value.replace("-", "_")
    except OSError:
        return None
    return None


def current_profile_and_role(config) -> tuple[str, str | None]:
    """Read the current (profile, role) from config + role manager.

    Returns a tuple where profile is ``"drone"`` or ``"ground-station"``
    (hyphen form, wire contract), and role is ``"direct" | "relay" |
    "receiver"`` for ground stations or ``None`` for drones.

    Resolution order:

    1. ``config.agent.profile`` when it is an explicit value
       (``"drone"`` / ``"ground_station"``).
    2. ``/etc/ados/profile.conf`` when the config field is ``"auto"``
       or empty. install.sh writes profile.conf during install
       and the operator can flip it via ``ados profile set``.
    3. ``"drone"`` as a final fallback.

    Without (2) the agent silently reports ``"drone"`` to the cloud
    and to LAN consumers any time the operator left ``agent.profile``
    on the ``"auto"`` default — which is the documented onboarding
    path. The fallback is what makes the auto-detected profile
    actually reach the wire.
    """
    raw = getattr(getattr(config, "agent", object()), "profile", None)
    if raw in (None, "", "auto"):
        raw = _read_profile_conf_value()
    profile = normalize_profile(raw)
    if profile == "ground-station":
        try:
            from ados.services.ground_station.role_manager import get_current_role
            role = get_current_role() or "direct"
        except Exception:
            role = None
    else:
        role = None
    return profile, role
