"""Per-service memory readback from the cgroup accounting systemd exposes.

The agent is a fleet of long-running ``ados-*.service`` units. Each one
runs in its own cgroup, and when the unit has ``MemoryAccounting=yes``
(set on the shared ``ados.slice`` the units join) systemd publishes the
live cgroup memory total as the ``MemoryCurrent`` property. Reading that
property gives an accurate per-service number (the kernel's cgroup
memory.current, close to PSS for a single-process unit) without parsing
``/proc`` or summing RSS by hand.

``systemctl show <unit> -p MemoryCurrent --value`` returns the byte count
as a decimal string, ``[not set]`` when the unit is not running, or the
u64 sentinel ``18446744073709551615`` when accounting is unavailable for
that unit. All three non-numeric cases map to ``0.0`` here so the caller
gets a clean float and never has to special-case the sentinel.

Everything in this module is best-effort and never raises: a missing
``systemctl`` binary, a subprocess error, or a timeout all resolve to
``0.0`` for the affected unit. The caller is expected to cache the result
(the status endpoints already memoize with a few-second TTL), so the
subprocess cost stays off the per-request hot path.
"""

from __future__ import annotations

import subprocess

from ados.core.logging import get_logger

log = get_logger("core.systemd_memory")

# u64 max — systemd reports this for a property whose accounting is not
# enabled on the unit. Treat it as "unknown", not a real 16 EiB reading.
_U64_MAX = 18446744073709551615
_SHOW_TIMEOUT_S = 5.0

# Map the in-process service short names (the asyncio task / ServiceTracker
# names used on the single-process demo path) onto the systemd unit that
# actually owns their cgroup on a stock multi-process install. Names absent
# here have no dedicated unit (e.g. the MAVLink proxy sockets run inside the
# mavlink unit) and resolve to None so the caller skips a pointless probe.
_SHORT_NAME_TO_UNIT: dict[str, str] = {
    "fc-connection": "ados-mavlink.service",
    "video-pipeline": "ados-video.service",
    "wfb-link": "ados-wfb.service",
    "rest-api": "ados-api.service",
    "scripting": "ados-scripting.service",
    "health-monitor": "ados-health.service",
    "cloud-command-poll": "ados-cloud.service",
    "agent-heartbeat": "ados-cloud.service",
    "pairing-beacon": "ados-cloud.service",
    "pairing-heartbeat": "ados-cloud.service",
    "ota-updater": "ados-ota.service",
}


def unit_for_service(name: str) -> str | None:
    """Resolve a services-list entry name to its systemd unit, or None.

    The consolidated status + services routes report entries from two
    sources: the systemd inventory fallback, whose names are already the
    unit basenames (``ados-video``), and the in-process ServiceTracker,
    whose names are short labels (``video-pipeline``). A unit-basename
    name maps to ``<name>.service``; a short label maps through the table
    above. Anything unrecognised returns None so the caller does not run a
    systemctl probe against a unit that does not exist.
    """
    if not name:
        return None
    if name.startswith("ados-"):
        return name if name.endswith(".service") else f"{name}.service"
    return _SHORT_NAME_TO_UNIT.get(name)


def _parse_memory_current(raw: str) -> float:
    """Convert a ``MemoryCurrent`` value (bytes) to MiB, rounded to 0.1.

    Returns ``0.0`` for the empty string, the ``[not set]`` marker, the
    u64 ``max`` sentinel, or any value that does not parse as an integer.
    """
    text = (raw or "").strip()
    if not text or text == "[not set]":
        return 0.0
    try:
        as_bytes = int(text)
    except ValueError:
        return 0.0
    if as_bytes < 0 or as_bytes >= _U64_MAX:
        return 0.0
    return round(as_bytes / (1024 * 1024), 1)


def service_memory_mb(unit: str) -> float:
    """Live memory use of a systemd unit in MiB (1 decimal); 0.0 on error.

    Reads ``MemoryCurrent`` from the unit's cgroup accounting. Requires
    ``MemoryAccounting=yes`` on the unit (the ados units join the shared
    ``ados.slice`` that sets it). When accounting is off, the unit is
    stopped, or the read fails, this returns 0.0. Never raises.
    """
    try:
        result = subprocess.run(
            ["systemctl", "show", unit, "-p", "MemoryCurrent", "--value"],
            capture_output=True,
            text=True,
            timeout=_SHOW_TIMEOUT_S,
            check=False,
        )
    except (OSError, subprocess.SubprocessError) as exc:
        log.debug("service_memory_read_failed", unit=unit, error=str(exc))
        return 0.0
    if result.returncode != 0:
        return 0.0
    return _parse_memory_current(result.stdout)


def services_memory_mb(units: list[str]) -> dict[str, float]:
    """Batch ``service_memory_mb`` over several units.

    One subprocess per unit. Cheap enough for the handful of ados units
    when the caller caches the dict behind the existing status TTL.
    Missing or unreadable units land at 0.0 rather than being dropped.
    """
    return {unit: service_memory_mb(unit) for unit in units}


__all__ = ["service_memory_mb", "services_memory_mb", "unit_for_service"]
