"""Shared constants + helpers for the setup routes."""

from __future__ import annotations

from datetime import datetime, timezone

from ados.core.logging import get_logger

log = get_logger("setup_api")

# Canonical step ids the wizard emits. Used to validate skip targets so
# operators cannot stash arbitrary keys in the state file.
VALID_STEP_IDS: frozenset[str] = frozenset(
    {
        "welcome",
        "profile",
        "region",
        "hardware_check",
        "cloud_choice",
        "pair",
        "mavlink",
        "video",
        "ground_receiver",
        "display",
        "remote_access",
        "finish",
    }
)


# Operator-facing one-shot prompts. The dashboard surfaces each id at
# most once per agent; the ack route lands the id in the persisted
# acked_nudges set so the next page load suppresses it. Keep the list
# small and well-known so a tampered request cannot stash arbitrary
# keys.
VALID_NUDGE_IDS: frozenset[str] = frozenset(
    {
        "cloud_posture_default_changed",
    }
)


def now_iso() -> str:
    """Tz-aware ISO timestamp without microseconds, matching wizard JSON shape."""
    return (
        datetime.now(timezone.utc).astimezone().replace(microsecond=0).isoformat()
    )
