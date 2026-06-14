"""Service status routes."""

from __future__ import annotations

import os
import subprocess
import time

from fastapi import APIRouter

from ados.core.logging import get_logger

log = get_logger("api.services")

router = APIRouter()

# Wildcard unit patterns we ask systemd about when the in-process
# tracker is empty. Covers every drone + ground-station + agent unit
# that ships on a stock install. Adding a new ados-* unit requires
# nothing here — the wildcard picks it up automatically.
_SYSTEMD_FALLBACK_PATTERNS = ("ados-*.service",)


def _systemd_inventory() -> tuple[list[dict], bool]:
    """Read the live unit list from systemd for every ados-* unit.

    Returns ``(entries, available)``. ``entries`` is a list of dicts the
    dashboard already knows how to render: ``{name, state, active,
    sub_state, pid}``. ``available`` is False when systemctl itself
    could not be reached (binary missing, subprocess error, timeout) —
    distinct from "systemd answered but no ados-* units exist". The
    dashboard uses ``available`` to render a different empty state for
    each case so operators can tell "no services running" from "could
    not query systemd."

    We force ``SYSTEMD_COLORS=0`` + ``--no-pager`` because the default
    failed-unit output starts each line with a status glyph (``● foo``,
    ``× bar``); a naive ``split()`` parser drops those lines because the
    first token is the glyph instead of the unit name — and failed units
    are exactly the rows the dashboard most needs to surface.
    """
    try:
        result = subprocess.run(
            [
                "systemctl",
                "list-units",
                "--type=service",
                "--all",
                "--no-legend",
                "--no-pager",
                "--plain",
                *_SYSTEMD_FALLBACK_PATTERNS,
            ],
            capture_output=True,
            text=True,
            timeout=5,
            env={"SYSTEMD_COLORS": "0", "SYSTEMD_PAGER": "", "LANG": "C"},
        )
    except (subprocess.SubprocessError, FileNotFoundError) as exc:
        log.warning("systemd_inventory_failed", error=str(exc))
        return ([], False)

    entries: list[dict] = []
    for line in (result.stdout or "").splitlines():
        stripped = line.strip()
        if not stripped:
            continue
        # Strip any leading status glyph (``●``, ``×``, ``*``) systemd
        # prepends to non-running units even with --no-legend.
        if not stripped[0].isalnum() and not stripped[0].isascii():
            stripped = stripped.split(None, 1)[-1] if " " in stripped else ""
        elif stripped[0] in "*●×":
            stripped = stripped.split(None, 1)[-1] if " " in stripped else ""
        parts = stripped.split(None, 4)
        if len(parts) < 4:
            continue
        unit, load_state, active_state, sub_state = parts[:4]
        if not unit.endswith(".service"):
            continue
        name = unit[: -len(".service")]
        entries.append(
            {
                "name": name,
                "active": active_state == "active",
                "state": active_state,
                "sub_state": sub_state,
                "pid": None,
                "load_state": load_state,
            }
        )
    return (entries, True)

# Cache process metrics (psutil is expensive to call per-request)
_proc_cache: dict = {"cpu": 0.0, "rss_mb": 0.0, "pid": 0, "ts": 0.0}


def _get_process_metrics() -> dict:
    """Get current process CPU% and RSS memory. Cached for 2 seconds."""
    now = time.monotonic()
    if now - _proc_cache["ts"] < 2.0 and _proc_cache["pid"] == os.getpid():
        return _proc_cache
    try:
        import psutil
        proc = psutil.Process(os.getpid())
        _proc_cache["cpu"] = proc.cpu_percent(interval=0)
        _proc_cache["rss_mb"] = proc.memory_info().rss / (1024 * 1024)
        _proc_cache["pid"] = os.getpid()
        _proc_cache["ts"] = now
    except Exception:
        pass
    return _proc_cache


async def _attach_service_memory(services: list[dict]) -> None:
    """Add a ``memory_mb`` field to every service entry, in place.

    Resolves each entry's owning systemd unit and writes that unit's PSS in
    MiB back onto every entry that maps to it. Entries with no dedicated unit
    (or a stopped / unknown unit) get ``0.0``. Resolved units are deduped so a
    unit shared by two entries is looked up once.

    Reads the durable store first: the supervisor's per-service sampler ships
    each unit's grouped PSS to the logging daemon continuously, so the route
    serves the same value from history without scanning ``/proc`` on every
    request. When the store is unreachable or has no sample yet (a fresh boot
    before the first tick), it falls back to the existing live ``/proc`` PSS
    scan. Both paths report identical MiB for the same underlying PSS, and a
    store gap degrades to the prior behavior, never to a 500.
    """
    from ados.api.sources.services import latest_service_memory
    from ados.core.systemd_memory import services_memory_mb, unit_for_service

    unit_by_entry: list[str | None] = [unit_for_service(s.get("name", "")) for s in services]
    distinct_units = sorted({u for u in unit_by_entry if u})

    by_unit: dict[str, float] = {}
    if distinct_units:
        # Store-first: the durable per-unit PSS series. None on any store gap.
        stored: dict[str, float] | None = None
        try:
            stored = await latest_service_memory()
        except Exception:  # noqa: BLE001 — a store read must never fault the route
            stored = None
        if stored:
            by_unit = {u: stored.get(u, 0.0) for u in distinct_units}
        else:
            # Live fallback: the on-demand /proc PSS scan, unchanged.
            by_unit = services_memory_mb(distinct_units)

    for svc, unit in zip(services, unit_by_entry):
        svc["memory_mb"] = by_unit.get(unit, 0.0) if unit else 0.0


def _infer_service_state(app, name: str, tracker_state: str, task_done: bool) -> str:
    """Infer true operational state from observable conditions.

    The tracker only knows running/stopped/failed, but many services
    are technically running (asyncio task alive) while functionally
    degraded (e.g. no FC connected, no camera, no WFB adapter).
    """
    if task_done or tracker_state in ("stopped", "failed"):
        return tracker_state

    # FC connection — degraded if no serial port / not connected
    if name == "fc-connection":
        fc = app.fc_connection()
        if fc and not getattr(fc, "connected", False):
            return "degraded"

    # Video pipeline — degraded if mode is disabled or no camera
    if name == "video-pipeline":
        if getattr(app.config.video, "mode", "disabled") == "disabled":
            return "stopped"

    # WFB link — degraded if no compatible adapter found
    if name == "wfb-link":
        wfb = app.wfb_manager()
        if wfb and not getattr(wfb, "has_adapter", False):
            return "degraded"

    # Pairing beacon — idle when already paired
    if name == "pairing-beacon":
        if app.pairing_manager.is_paired:
            return "stopped"

    return tracker_state

