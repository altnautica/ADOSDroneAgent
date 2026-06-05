"""Per-service memory readback, grouped by the systemd cgroup each process runs in.

The agent is a fleet of long-running ``ados-*.service`` units. The obvious
way to get per-service memory is systemd's ``MemoryCurrent`` cgroup property,
but that needs the kernel **memory cgroup controller**, which is disabled by
default on Raspberry Pi OS (it requires ``cgroup_enable=memory`` on the boot
cmdline plus a reboot). On such a board ``MemoryCurrent`` reads ``[not set]``
for every unit regardless of ``MemoryAccounting=yes``.

So this module derives per-service memory from ``/proc`` instead, which works
on every board with no boot parameter and no reboot: for each running PID it
reads the owning systemd unit from ``/proc/<pid>/cgroup`` and sums the process's
**PSS** (proportional set size) from ``/proc/<pid>/smaps_rollup``. PSS divides
shared pages (e.g. one ``libpython`` across several Python services) fairly
across the processes that map them, so the per-service totals add up sensibly
and a multi-process unit (``ados-video`` = the orchestrator plus its ffmpeg and
mediamtx children) is summed correctly.

Everything here is best-effort and never raises: an unreadable ``/proc`` entry,
a PID that exits mid-scan, or no read permission all resolve to skipping that
process. Reading another process's ``smaps_rollup`` needs root (the agent runs
as root); without it the affected processes contribute 0 and the feature
degrades gracefully rather than erroring. The result is cached for a few seconds
so a single status build that asks for several units scans ``/proc`` only once.
"""

from __future__ import annotations

import os
import re
import time

from ados.core.logging import get_logger

log = get_logger("core.systemd_memory")

# A process's cgroup line names its systemd unit, e.g.
#   0::/system.slice/ados.slice/ados-video.service
_UNIT_RE = re.compile(r"(ados-[a-z0-9-]+\.service)")

# Re-scan /proc at most this often; the status routes already memoize their
# whole payload with a few-second TTL, this just collapses repeat calls within
# one build.
_CACHE_TTL_S = 3.0

_cache: dict[str, float] | None = None
_cache_ts = 0.0

# Map the in-process service short names (the asyncio task / ServiceTracker
# names used on the single-process demo path) onto the systemd unit that owns
# their cgroup on a stock multi-process install. Names absent here have no
# dedicated unit and resolve to None so the caller does not look them up.
_SHORT_NAME_TO_UNIT: dict[str, str] = {
    "fc-connection": "ados-mavlink.service",
    "video-pipeline": "ados-video.service",
    "wfb-link": "ados-wfb.service",
    "rest-api": "ados-api.service",
    "health-monitor": "ados-health.service",
    "cloud-command-poll": "ados-cloud.service",
    "agent-heartbeat": "ados-cloud.service",
    "pairing-beacon": "ados-cloud.service",
    "pairing-heartbeat": "ados-cloud.service",
    "ota-updater": "ados-ota.service",
}


def unit_for_service(name: str) -> str | None:
    """Resolve a services-list entry name to its systemd unit, or None.

    A unit-basename name (``ados-video``) maps to ``<name>.service``; a short
    in-process label (``video-pipeline``) maps through the table above.
    Anything unrecognised returns None.
    """
    if not name:
        return None
    if name.startswith("ados-"):
        return name if name.endswith(".service") else f"{name}.service"
    return _SHORT_NAME_TO_UNIT.get(name)


def unit_from_cgroup(text: str) -> str | None:
    """Extract the ``ados-*.service`` unit from a ``/proc/<pid>/cgroup`` body.

    Pure + testable. Returns None when no ados unit appears (the process
    belongs to some other slice, or to no unit at all).
    """
    match = _UNIT_RE.search(text or "")
    return match.group(1) if match else None


def pss_kib_from_rollup(text: str) -> int:
    """Parse the ``Pss:`` line out of a ``/proc/<pid>/smaps_rollup`` body (KiB).

    Pure + testable. Returns 0 when the rollup has no ``Pss:`` line (older
    kernels) or it does not parse.
    """
    for line in (text or "").splitlines():
        if line.startswith("Pss:"):
            parts = line.split()
            if len(parts) >= 2 and parts[1].isdigit():
                return int(parts[1])
            return 0
    return 0


def _scan_pss_by_unit() -> dict[str, float]:
    """Sum PSS (MiB, 1 decimal) per ados unit across all running PIDs."""
    totals_kib: dict[str, int] = {}
    try:
        pids = [e.name for e in os.scandir("/proc") if e.name.isdigit()]
    except OSError as exc:
        log.debug("proc_scan_failed", error=str(exc))
        return {}
    for pid in pids:
        try:
            with open(f"/proc/{pid}/cgroup", encoding="utf-8") as fh:
                unit = unit_from_cgroup(fh.read())
        except (OSError, UnicodeDecodeError):
            continue
        if unit is None:
            continue
        try:
            with open(f"/proc/{pid}/smaps_rollup", encoding="utf-8") as fh:
                pss = pss_kib_from_rollup(fh.read())
        except (OSError, UnicodeDecodeError):
            # PID exited mid-scan, or no read permission (not root): skip.
            continue
        if pss:
            totals_kib[unit] = totals_kib.get(unit, 0) + pss
    return {unit: round(kib / 1024, 1) for unit, kib in totals_kib.items()}


def _pss_map() -> dict[str, float]:
    """Cached per-unit PSS map (MiB). Re-scans /proc at most every few seconds."""
    global _cache, _cache_ts
    now = time.monotonic()
    if _cache is not None and (now - _cache_ts) < _CACHE_TTL_S:
        return _cache
    _cache = _scan_pss_by_unit()
    _cache_ts = now
    return _cache


def service_memory_mb(unit: str) -> float:
    """PSS memory of a systemd unit (all its processes) in MiB; 0.0 if unknown."""
    return _pss_map().get(unit, 0.0)


def services_memory_mb(units: list[str]) -> dict[str, float]:
    """Batch ``service_memory_mb`` over several units with a single /proc scan.

    Units with no running processes (or unreadable) land at 0.0 rather than
    being dropped, so the caller always gets an entry for every requested unit.
    """
    pss = _pss_map()
    return {unit: pss.get(unit, 0.0) for unit in units}


__all__ = [
    "pss_kib_from_rollup",
    "service_memory_mb",
    "services_memory_mb",
    "unit_for_service",
    "unit_from_cgroup",
]
