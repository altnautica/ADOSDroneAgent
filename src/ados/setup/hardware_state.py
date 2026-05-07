"""Persistent hardware-check snapshot.

The agent re-probes hardware (rpicam-hello, v4l2-ctl, lsusb, modem
detection, GPIO) on every call to ``run_hardware_check()``. The
dashboard polls ``/api/v1/setup/status`` every 8 s, which means the
agent was burning measurable CPU + USB enumeration cycles on data
that almost never changes between polls.

This module owns a single JSON snapshot at
``/var/ados/setup/hardware-state.json``. The cached runner in
``hardware_check.py`` reads from here when the snapshot is fresh
(default 30 s TTL) and the (profile, ground_role) match the active
config; otherwise it falls back to a fresh probe and persists the
result.

Hot-plug invalidation lands in v0.16.3 (udev rules + a CLI
``ados hardware bust-cache`` subcommand). For now, manual operator
``Rescan`` from the dashboard always probes fresh.
"""

from __future__ import annotations

import json
import os
from datetime import datetime, timezone
from pathlib import Path

from ados.core.logging import get_logger
from ados.core.paths import HARDWARE_STATE_PATH, SETUP_STATE_DIR
from ados.setup.models import HardwareCheckStatus

log = get_logger("setup.hardware_state")

# How long a snapshot stays fresh before the cached runner falls
# back to a probe. 30 s is short enough that operator testing
# isn't blocked by stale data and long enough to flatten dashboard
# polling spikes (8 s poll interval -> 4 cache hits per snapshot).
DEFAULT_TTL_SECONDS = 30


def read() -> HardwareCheckStatus | None:
    """Return the persisted snapshot or ``None`` if absent / unreadable."""
    if not HARDWARE_STATE_PATH.is_file():
        return None
    try:
        raw = HARDWARE_STATE_PATH.read_text(encoding="utf-8")
        data = json.loads(raw)
        return HardwareCheckStatus.model_validate(data)
    except (OSError, json.JSONDecodeError, ValueError) as exc:
        log.warning("hardware_state_read_failed", error=str(exc))
        return None


def write(status: HardwareCheckStatus) -> None:
    """Best-effort atomic persistence.

    Failures are logged but never raised: a missing /var/ados (e.g.
    on a dev workstation running tests) must not break the API
    layer that just wanted to populate the cache.
    """
    try:
        SETUP_STATE_DIR.mkdir(parents=True, exist_ok=True)
    except OSError as exc:
        log.warning("hardware_state_mkdir_failed", error=str(exc))
        return
    tmp_path = HARDWARE_STATE_PATH.with_suffix(
        HARDWARE_STATE_PATH.suffix + ".tmp"
    )
    payload = status.model_dump(mode="json")
    try:
        tmp_path.write_text(
            json.dumps(payload, sort_keys=True) + "\n", encoding="utf-8"
        )
        os.replace(tmp_path, HARDWARE_STATE_PATH)
        # Snapshot is operator-readable so the GCS user (which
        # reads the API but doesn't run as root) can introspect
        # if it ever needs to. Writing happens only by the agent.
        try:
            os.chmod(HARDWARE_STATE_PATH, 0o644)
        except OSError:
            # Filesystem may not support chmod (e.g. tests on tmpfs);
            # not fatal.
            pass
    except OSError as exc:
        log.warning("hardware_state_write_failed", error=str(exc))


def is_fresh(
    status: HardwareCheckStatus,
    *,
    ttl_seconds: int = DEFAULT_TTL_SECONDS,
) -> bool:
    """True when the snapshot's ``last_run`` is within ``ttl_seconds``."""
    if not status.last_run:
        return False
    try:
        ts = datetime.fromisoformat(status.last_run)
        if ts.tzinfo is None:
            ts = ts.replace(tzinfo=timezone.utc)
    except ValueError:
        return False
    age = (datetime.now(tz=ts.tzinfo) - ts).total_seconds()
    return age >= 0 and age < ttl_seconds


def matches(
    status: HardwareCheckStatus,
    *,
    profile: str,
    ground_role: str,
) -> bool:
    """True when the cached snapshot was taken under the same profile.

    A profile or role swap (drone <-> ground_station, direct <-> relay)
    must invalidate the cache because the per-profile probe set is
    different.
    """
    return (
        status.profile == profile
        and status.ground_role == ground_role
    )


def clear() -> None:
    """Delete the persisted snapshot. Used by tests + future hot-plug."""
    try:
        HARDWARE_STATE_PATH.unlink(missing_ok=True)
    except OSError as exc:
        log.warning("hardware_state_clear_failed", error=str(exc))


# Test seam: tests can monkeypatch this to redirect persistence under
# a tmp_path tree without monkeypatching the path constants.
def _state_path() -> Path:
    return HARDWARE_STATE_PATH
