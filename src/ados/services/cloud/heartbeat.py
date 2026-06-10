"""Pure helpers folded into the cloud heartbeat payload.

The periodic status heartbeat that Mission Control reads to populate
the per-drone capability store is assembled by
``ados.core.main.heartbeat_payload.build_heartbeat_payload``. That
assembler folds in a few small, pure source helpers that read the
node's runtime mode and stable-MAC pin state and reduce the result to
a dict-shaped fragment.

This module owns those helpers. They are also reused by the REST
network surface so the heartbeat block and the API read one source of
truth for the MAC-pin state.
"""

from __future__ import annotations

import json
from typing import Any


def build_runtime_mode_enrichment(config: Any) -> dict:
    """Resolve the node's native-vs-packaged runtime mode for the heartbeat.

    Returns ``{"runtimeMode": "native" | "hybrid" | "packaged"}`` so the
    GCS can show a per-node badge for how much of the long-running
    service set runs the native binaries vs the packaged Python ones.
    The aggregate is profile-aware: a drone is judged on the services it
    actually starts, a ground station on its own set.

    Best-effort and total: any failure resolving the profile falls back
    to the drone set, and the underlying check never raises. The caller
    does ``payload.update(...)``.
    """
    from ados.core.profile import current_profile_and_role
    from ados.core.runtime_mode import compute_runtime_mode

    try:
        profile, _ = current_profile_and_role(config)
    except Exception:
        profile = "drone"
    return {"runtimeMode": compute_runtime_mode(profile)}


def read_mac_pins_state() -> dict | None:
    """Read ``/etc/ados/mac-pins.state`` (written by the Rust installer step +
    supervisor reconciler). Returns the parsed document, or ``None`` when the
    file is absent or malformed. Shared by the heartbeat enricher and the REST
    surface so both read one source of truth.
    """
    from ados.core.paths import ADOS_ETC_DIR

    try:
        return json.loads((ADOS_ETC_DIR / "mac-pins.state").read_text())
    except (OSError, ValueError):
        return None


def _mac_adapter_to_camel(a: dict) -> dict:
    """Map one snake_case state-file adapter to the camelCase shape the GCS
    expects (mirrors how the other heartbeat blocks are camelCased)."""
    out: dict = {
        "name": a.get("name"),
        "vidpid": a.get("vidpid"),
        "usbPath": a.get("usb_path"),
        "state": a.get("state"),
        "appliedLive": bool(a.get("applied_live", False)),
    }
    for src, dst in (
        ("source", "source"),
        ("pinned_mac", "pinnedMac"),
        ("last_seen_mac", "lastSeenMac"),
        ("link_file", "linkFile"),
        ("deferred_reason", "deferredReason"),
    ):
        if a.get(src) is not None:
            out[dst] = a.get(src)
    return out


def build_mac_stability_enrichment() -> dict:
    """Surface stable-MAC pin verdicts for the GCS Network panel.

    Returns ``{"macStability": {"version": N, "adapters": [...]}}`` when at
    least one adapter is not plainly stable (a randomizer was pinned, is a
    learner candidate, or a pin is deferred). Returns ``{}`` on a board with no
    such adapter, so the caller's ``payload.update`` is a no-op (omit-when-
    absent, like the display enricher). Best-effort: any failure returns ``{}``.
    """
    raw = read_mac_pins_state()
    if not raw:
        return {}
    adapters = raw.get("adapters")
    if not isinstance(adapters, list) or not adapters:
        return {}
    adapters = [a for a in adapters if isinstance(a, dict)]
    # An all-stable board is boring; only surface when something needs attention.
    if all(a.get("state") == "stable" for a in adapters):
        return {}
    return {
        "macStability": {
            "version": raw.get("version", 1),
            "adapters": [_mac_adapter_to_camel(a) for a in adapters],
        }
    }
