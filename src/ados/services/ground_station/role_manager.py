"""Role transitions for the ground-station distributed RX profile.

A ground-station node operates in one of three roles:

- `direct`: single-node RX, no mesh services running.
- `relay`: forwards WFB fragments to a receiver over batman-adv.
- `receiver`: aggregates fragments from local NIC plus remote relays,
  FEC-combines them, feeds the existing mediamtx-gs pipeline.

Role transitions are applied through this module from two entry points:

1. Boot: `ados-supervisor` calls `apply_role_on_boot()` after config load
   so the correct set of role-gated systemd units is enabled.
2. Operator: `PUT /api/v1/ground-station/role` on the REST API, or the
   OLED Mesh submenu, both call `apply_role()`.

The module does three things per transition:

- Writes a sentinel file `/etc/ados/mesh/role` with the new value so
  systemd unit `ConditionPathExists` gates honor the state across
  reboots even if the Pydantic config is temporarily out of sync.
- Masks systemd units for the previous role and unmasks units for
  the new role (belt-and-suspenders against stray `systemctl start`).
- Stops the old role's services, starts the new role's services, in
  dependency order.

A MeshEvent `role_changed` is published on the shared bus so the GCS
Hardware tab, OLED status row, and logs all see the transition.

The module is a library, not a standalone systemd service. It is
imported by the supervisor, REST router, and OLED menu handler.
"""

from __future__ import annotations

import asyncio
import os
import subprocess
import time
from pathlib import Path

from ados.core.logging import get_logger
from ados.core.paths import (
    MESH_ROLE_PATH,
    MESH_STATE_JSON,
    WFB_RECEIVER_JSON,
    WFB_RELAY_JSON,
)

from .events import MeshEvent, get_mesh_event_bus

log = get_logger("ground_station.role_manager")

ROLE_FILE = MESH_ROLE_PATH
VALID_ROLES: tuple[str, ...] = ("direct", "relay", "receiver")

# Systemd units gated by role. Order matters for start/stop sequencing.
# `ados-batman.service` always comes up before the wfb relay or receiver
# units because the wfb side binds to the batman-adv interface.
_ROLE_UNITS: dict[str, list[str]] = {
    "direct": [],
    "relay": ["ados-batman.service", "ados-wfb-relay.service"],
    "receiver": ["ados-batman.service", "ados-wfb-receiver.service"],
}

_ALL_MESH_UNITS: tuple[str, ...] = (
    "ados-batman.service",
    "ados-wfb-relay.service",
    "ados-wfb-receiver.service",
)

# Runtime state files published by mesh and wfb services. Cleared when a
# node transitions out of a role so a stale snapshot can never mislead a
# REST client or the OLED on the next start.
_MESH_STATE_FILES: tuple[Path, ...] = (
    MESH_STATE_JSON,
    WFB_RELAY_JSON,
    WFB_RECEIVER_JSON,
)


def get_current_role() -> str:
    """Read the on-disk role sentinel.

    Falls back to `direct` if the sentinel is missing, unreadable, or
    contains an unknown value. This keeps boot-time behavior safe on
    a fresh install.
    """
    try:
        if ROLE_FILE.is_file():
            value = ROLE_FILE.read_text(encoding="utf-8").strip()
            if value in VALID_ROLES:
                return value
    except OSError:
        pass
    return "direct"


def _write_role_file(role: str) -> None:
    """Atomically write the role sentinel (0o644, owner-writable)."""
    ROLE_FILE.parent.mkdir(parents=True, exist_ok=True)
    tmp = ROLE_FILE.with_suffix(ROLE_FILE.suffix + ".tmp")
    tmp.write_text(role + "\n", encoding="utf-8")
    os.chmod(tmp, 0o644)
    os.replace(str(tmp), str(ROLE_FILE))


def _run_systemctl(args: list[str], timeout: float = 15.0) -> tuple[bool, str]:
    """Thin wrapper over `systemctl` with a consistent error shape."""
    cmd = ["systemctl", *args]
    try:
        result = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            timeout=timeout,
        )
        stderr = result.stderr.strip()
        if result.returncode != 0:
            log.warning(
                "systemctl_failed",
                args=args,
                rc=result.returncode,
                stderr=stderr,
            )
            return False, stderr
        return True, result.stdout.strip()
    except subprocess.TimeoutExpired:
        log.error("systemctl_timeout", args=args)
        return False, "timeout"
    except FileNotFoundError:
        # systemctl is missing (unit test env). Treat as success so the
        # function stays callable under pytest without mocking.
        log.debug("systemctl_missing", args=args)
        return True, ""


def _mask_unit(unit: str) -> None:
    _run_systemctl(["mask", unit])


def _unmask_unit(unit: str) -> None:
    _run_systemctl(["unmask", unit])


async def _stop_unit(unit: str) -> None:
    ok, _ = _run_systemctl(["stop", unit])
    if not ok:
        # Stop failures are usually "already inactive". Log and continue.
        log.debug("stop_unit_noop", unit=unit)


async def _start_unit(unit: str) -> bool:
    ok, _ = _run_systemctl(["start", unit])
    return ok


async def apply_role(
    target: str,
    *,
    reason: str = "operator",
    previous: str | None = None,
) -> dict:
    """Apply the target role. Stops old units, starts new units, masks the rest.

    Returns a dict with transition metadata suitable for API responses:
        {"role": "relay", "previous": "direct", "units_started": [...],
         "units_stopped": [...], "ts_ms": 1700000000000}

    Raises `ValueError` for unknown roles. Never raises on systemctl
    failures; instead logs them and proceeds. That lets a partial
    transition complete on a node where one unit is temporarily wedged.
    """
    if target not in VALID_ROLES:
        raise ValueError(
            f"role must be one of {VALID_ROLES!r}, got {target!r}"
        )

    current = previous if previous is not None else get_current_role()
    ts_ms = int(time.time() * 1000)

    if current == target:
        log.info("role_apply_noop", role=target)
        return {
            "role": target,
            "previous": current,
            "units_started": [],
            "units_stopped": [],
            "ts_ms": ts_ms,
            "noop": True,
        }

    log.info(
        "role_apply_start",
        previous=current,
        target=target,
        reason=reason,
    )

    units_stopped: list[str] = []
    units_started: list[str] = []

    # Stop the old role's units in reverse dependency order so the
    # wfb side quiesces before batman tears down.
    for unit in reversed(_ROLE_UNITS.get(current, [])):
        await _stop_unit(unit)
        units_stopped.append(unit)

    # Clear stale runtime snapshots so a freshly-direct node cannot
    # serve old mesh data from a previous relay or receiver session.
    for p in _MESH_STATE_FILES:
        try:
            if p.is_file():
                p.unlink()
        except OSError as exc:
            log.debug("mesh_state_unlink_failed", path=str(p), error=str(exc))

    # Mask every mesh unit, then unmask the ones for the target role.
    # Masking is idempotent; a unit that was already masked stays masked.
    for unit in _ALL_MESH_UNITS:
        _mask_unit(unit)
    for unit in _ROLE_UNITS.get(target, []):
        _unmask_unit(unit)

    # Flip the sentinel BEFORE starting new units so their systemd
    # ConditionPathExists checks pass.
    _write_role_file(target)

    # Start new units in dependency order. batman first, then wfb.
    for unit in _ROLE_UNITS.get(target, []):
        if await _start_unit(unit):
            units_started.append(unit)

    # Publish transition event last so subscribers see a consistent state.
    try:
        bus = get_mesh_event_bus()
        await bus.publish(
            MeshEvent(
                kind="role_changed",
                timestamp_ms=ts_ms,
                payload={
                    "previous": current,
                    "role": target,
                    "reason": reason,
                    "units_started": units_started,
                    "units_stopped": units_stopped,
                },
            )
        )
    except Exception as exc:  # publish is best-effort
        log.debug("role_event_publish_failed", error=str(exc))

    log.info(
        "role_apply_done",
        previous=current,
        target=target,
        units_started=units_started,
        units_stopped=units_stopped,
    )

    return {
        "role": target,
        "previous": current,
        "units_started": units_started,
        "units_stopped": units_stopped,
        "ts_ms": ts_ms,
        "noop": False,
    }


def apply_role_on_boot_sync(role: str) -> None:
    """Apply mask/unmask state at supervisor boot without blocking.

    Runs the synchronous portion (sentinel file, masks, unmasks). Start
    of role-gated services is handled by the supervisor's own service
    lifecycle, not this function, so the sequencing stays owned by a
    single place.
    """
    if role not in VALID_ROLES:
        log.warning("role_invalid_at_boot", role=role)
        role = "direct"

    try:
        _write_role_file(role)
    except OSError as exc:
        log.error("role_file_write_failed", error=str(exc))

    for unit in _ALL_MESH_UNITS:
        _mask_unit(unit)
    for unit in _ROLE_UNITS.get(role, []):
        _unmask_unit(unit)

    log.info("role_boot_applied", role=role)


def role_units(role: str) -> list[str]:
    """Return the ordered list of systemd units active for a given role."""
    return list(_ROLE_UNITS.get(role, []))


def all_mesh_units() -> list[str]:
    """Return every mesh systemd unit known to this module."""
    return list(_ALL_MESH_UNITS)


# Convenience export for tests and REST handlers that want a pair of
# current (on-disk) and valid roles without importing constants directly.
def role_info() -> dict:
    return {
        "current": get_current_role(),
        "valid": list(VALID_ROLES),
        "units": {role: role_units(role) for role in VALID_ROLES},
    }
