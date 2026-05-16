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
        "hardware_check",
        "navigation",
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


def now_iso() -> str:
    """Tz-aware ISO timestamp without microseconds, matching wizard JSON shape."""
    return (
        datetime.now(timezone.utc).astimezone().replace(microsecond=0).isoformat()
    )
