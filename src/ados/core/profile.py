"""Profile and role helpers for the wire contract.

The agent stores ``profile`` as ``"drone"`` or ``"ground_station"``
internally (underscore form, set by ``bootstrap/profile_detect.py`` and
read from ``/etc/ados/profile.conf``). The GCS-facing wire contract uses
the hyphenated form ``"ground-station"``. This module is the single
place the two forms are bridged so the heartbeat, the pairing-info
endpoint, and the mDNS TXT records all emit the same shape.
"""

from __future__ import annotations


def normalize_profile(raw: str | None) -> str:
    """Return the wire-contract profile string."""
    if raw == "ground_station":
        return "ground-station"
    # "drone", "auto", "", None → "drone" for wire purposes.
    return "drone"


def current_profile_and_role(config) -> tuple[str, str | None]:
    """Read the current (profile, role) from config + role manager.

    Returns a tuple where profile is ``"drone"`` or ``"ground-station"``
    (hyphen form, wire contract), and role is ``"direct" | "relay" |
    "receiver"`` for ground stations or ``None`` for drones.
    """
    raw = getattr(getattr(config, "agent", object()), "profile", None)
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
