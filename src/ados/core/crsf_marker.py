"""CRSF RC-lane enable-marker reconcile + service kick.

The ``ados-crsf`` systemd unit gates on ``/etc/ados/crsf-enabled``
(``ConditionPathExists``), and the marker mirrors ``radio.crsf.enabled`` —
it is never hand-managed. Two writers keep it true to the config:

* the installer (its systemd step reconciles the marker from the on-disk
  config on every install/upgrade), and
* the runtime config persist path (this module, called from the single
  ``/etc/ados/config.yaml`` write chokepoint), so enabling the lane through
  any config surface is sufficient — the marker lands and the unit is
  kicked with no manual ``systemctl``.

The kick uses ``systemctl --no-block reload-or-restart``: a running lane
re-reads its config in place (the service's SIGHUP reload), a
condition-skipped unit re-evaluates the now-present marker and starts, and
a disable leaves the running service idling honestly (its own opt-in gate
reads the fresh config) until the next boot skips the unit entirely.

The MAVLink router reads the same ``radio.crsf`` block — the pinned
``device`` is excluded from FC port candidacy in ``crsf_rc`` mode, and in
``mavlink`` mode the block RESOLVES the router's MAVLink-over-ELRS ingest
source (the module is a plain MAVLink byte pipe the router owns) — but the
router loads its config once at startup, with no SIGHUP reload. So when the
router-relevant projection of the slice changes (the pin, or the resolved
ingest source), ``ados-mavlink.service`` gets a ``--no-block try-restart``:
restart-if-running, never force-starting a unit the profile keeps stopped.
Lane-only knobs (packet rate, band, channel source, TX power) never churn
the FC link.

Everything here is best-effort: a marker or systemctl failure is logged and
never fails the config write that triggered it.
"""

from __future__ import annotations

import subprocess
from typing import Any

from ados.core.logging import get_logger

log = get_logger("crsf_marker")

_SYSTEMCTL_TIMEOUT_S = 10.0


def _crsf_slice(config: dict[str, Any] | None) -> dict[str, Any]:
    """The ``radio.crsf`` mapping of a raw config dict (``{}`` when absent)."""
    if not isinstance(config, dict):
        return {}
    radio = config.get("radio")
    if not isinstance(radio, dict):
        return {}
    crsf = radio.get("crsf")
    return crsf if isinstance(crsf, dict) else {}


def reconcile_crsf_marker(config: dict[str, Any] | None) -> bool:
    """Mirror ``radio.crsf.enabled`` onto the enable marker.

    Returns True when the marker's presence CHANGED (used by callers that
    only act on a flip). Best-effort: an OSError is logged and reported as
    no-change so the caller's config write is never failed by the marker.
    """
    from ados.core.paths import CRSF_ENABLED_PATH

    enabled = bool(_crsf_slice(config).get("enabled", False))
    try:
        exists = CRSF_ENABLED_PATH.exists()
        if enabled and not exists:
            CRSF_ENABLED_PATH.touch()
            log.info("crsf_marker_written", path=str(CRSF_ENABLED_PATH))
            return True
        if not enabled and exists:
            CRSF_ENABLED_PATH.unlink()
            log.info("crsf_marker_removed", path=str(CRSF_ENABLED_PATH))
            return True
    except OSError as exc:
        log.warning("crsf_marker_reconcile_failed", error=str(exc))
    return False


def _kick_crsf_service() -> None:
    """Fire-and-forget ``reload-or-restart`` of the lane unit.

    ``--no-block`` queues the job without waiting, so a config-write request
    is never held on systemd; failures (no systemctl on a dev host, unit not
    installed) are logged at debug and swallowed.
    """
    try:
        subprocess.run(
            ["systemctl", "--no-block", "reload-or-restart", "ados-crsf.service"],
            capture_output=True,
            timeout=_SYSTEMCTL_TIMEOUT_S,
            check=False,
        )
    except (OSError, subprocess.SubprocessError) as exc:
        log.debug("crsf_service_kick_failed", error=str(exc))


def _router_view(crsf: dict[str, Any]) -> tuple[Any, Any]:
    """The projection of the lane slice the MAVLink router consumes.

    Two inputs decide the router's behaviour: the pinned ``device`` (excluded
    from FC port candidacy in ``crsf_rc`` mode; opened as the FC source in
    MAVLink mode) and the resolved MAVLink-over-ELRS ingest source —
    ``enabled`` + ``mode: mavlink`` + the carrier (``backpack_wifi``, else
    the serial default, matching the router's own parse of the block). The
    router loads its config once at startup, so only a change to THIS view
    warrants a unit restart; every other lane knob (packet rate, band,
    channel source, TX power) is the lane's own business and must never
    churn the FC link.
    """
    device = crsf.get("device")
    if bool(crsf.get("enabled", False)) and crsf.get("mode") == "mavlink":
        transport = crsf.get("mavlink_transport")
        source = "backpack_wifi" if transport == "backpack_wifi" else "serial"
    else:
        source = None
    return (device, source)


def _kick_mavlink_router() -> None:
    """Fire-and-forget ``try-restart`` of the MAVLink router unit.

    ``try-restart`` restarts only a running unit — a profile that keeps the
    router stopped is never force-started by a config save. The router has
    no in-process reload, so a restart is how the fresh ``radio.crsf`` view
    (the pin exclusion / the MAVLink-over-ELRS source) takes effect.
    Best-effort like the lane kick.
    """
    try:
        subprocess.run(
            ["systemctl", "--no-block", "try-restart", "ados-mavlink.service"],
            capture_output=True,
            timeout=_SYSTEMCTL_TIMEOUT_S,
            check=False,
        )
    except (OSError, subprocess.SubprocessError) as exc:
        log.debug("mavlink_router_kick_failed", error=str(exc))


def sync_after_config_write(
    previous: dict[str, Any] | None, current: dict[str, Any] | None
) -> None:
    """Reconcile the marker + kick the affected units after a config write.

    ``previous`` is the config as it was on disk before the write (``None``
    when unknown/absent), ``current`` the just-persisted dict. The marker is
    reconciled on every write (idempotent, self-healing); the lane unit is
    kicked only when the ``radio.crsf`` slice actually changed, and the
    MAVLink router only when the router-relevant projection of that slice
    changed (the pin, or the resolved MAVLink-over-ELRS source), so
    unrelated config saves never churn either service. Best-effort
    throughout.
    """
    marker_changed = reconcile_crsf_marker(current)
    prev_slice = _crsf_slice(previous)
    cur_slice = _crsf_slice(current)
    if marker_changed or prev_slice != cur_slice:
        _kick_crsf_service()
    if _router_view(prev_slice) != _router_view(cur_slice):
        _kick_mavlink_router()
