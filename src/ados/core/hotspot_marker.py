"""Ground-station setup-AP enable-marker reconcile + service kick.

The ``ados-dnsmasq-gs`` systemd unit gates on ``/etc/ados/hotspot-enabled``
(``ConditionPathExists``), and the marker mirrors ``network.hotspot.enabled``
— it is never hand-managed. Two writers keep it true to the config:

* the installer (its systemd step reconciles the marker from the on-disk
  config on every install/upgrade), and
* the runtime config persist path (this module, called from the single
  ``/etc/ados/config.yaml`` write chokepoint), so toggling the hotspot
  through any config surface is sufficient — the marker lands and the
  units are kicked with no manual ``systemctl``.

Why the DHCP/DNS unit needs its own marker rather than riding hostapd's
unit state: when the operator has not opted in, the hostapd service
*idles in place* instead of exiting, so that a ``Type=simple`` exit is
never misread by the supervisor as a crash-and-retry. That leaves
``ados-hostapd.service`` in ``active`` state, which satisfies dnsmasq's
``Requires=`` — so dnsmasq starts, tries to bind the AP address on an
interface that (on a ground station using its onboard radio as a WiFi
client) already carries a DHCP lease, fails, and burns its restart budget
into a permanently failed unit on an otherwise healthy box.

Both units are kicked on a flip: dnsmasq so a newly-present marker starts
the condition-skipped unit (and a newly-absent one stops it), hostapd so
its own opt-in gate re-reads the fresh config rather than idling until
the next boot.

Everything here is best-effort: a marker or systemctl failure is logged
and never fails the config write that triggered it.
"""

from __future__ import annotations

import subprocess
from typing import Any

from ados.core.logging import get_logger

log = get_logger("hotspot_marker")

_SYSTEMCTL_TIMEOUT_S = 10.0


def _hotspot_slice(config: dict[str, Any] | None) -> dict[str, Any]:
    """The ``network.hotspot`` mapping of a raw config dict (``{}`` absent)."""
    if not isinstance(config, dict):
        return {}
    network = config.get("network")
    if not isinstance(network, dict):
        return {}
    hotspot = network.get("hotspot")
    return hotspot if isinstance(hotspot, dict) else {}


def reconcile_hotspot_marker(config: dict[str, Any] | None) -> bool:
    """Mirror ``network.hotspot.enabled`` onto the enable marker.

    Returns True when the marker's presence CHANGED (used by callers that
    only act on a flip). Best-effort: an OSError is logged and reported as
    no-change so the caller's config write is never failed by the marker.
    """
    from ados.core.paths import HOTSPOT_ENABLED_PATH

    enabled = bool(_hotspot_slice(config).get("enabled", False))
    try:
        exists = HOTSPOT_ENABLED_PATH.exists()
        if enabled and not exists:
            HOTSPOT_ENABLED_PATH.touch()
            log.info("hotspot_marker_written", path=str(HOTSPOT_ENABLED_PATH))
            return True
        if not enabled and exists:
            HOTSPOT_ENABLED_PATH.unlink()
            log.info("hotspot_marker_removed", path=str(HOTSPOT_ENABLED_PATH))
            return True
    except OSError as exc:
        log.warning("hotspot_marker_reconcile_failed", error=str(exc))
    return False


def _kick(unit: str, verb: str) -> None:
    """Fire-and-forget ``systemctl`` on a ground-station AP unit.

    ``--no-block`` queues the job without waiting, so a config-write
    request is never held on systemd; failures (no systemctl on a dev
    host, unit not installed) are logged at debug and swallowed.
    """
    try:
        subprocess.run(
            ["systemctl", "--no-block", verb, unit],
            capture_output=True,
            timeout=_SYSTEMCTL_TIMEOUT_S,
            check=False,
        )
    except (OSError, subprocess.SubprocessError) as exc:
        log.debug("hotspot_unit_kick_failed", unit=unit, verb=verb, error=str(exc))


def sync_after_config_write(
    previous: dict[str, Any] | None, current: dict[str, Any] | None
) -> None:
    """Reconcile the marker + kick the AP units after a config write.

    ``previous`` is the config as it was on disk before the write
    (``None`` when unknown/absent), ``current`` the just-persisted dict.
    The marker is reconciled on every write (idempotent, self-healing);
    the units are kicked only when the ``network.hotspot`` slice actually
    changed, so unrelated config saves never churn a running AP.

    dnsmasq gets ``reload-or-restart`` — a condition-skipped unit
    re-evaluates the now-present marker and starts; hostapd gets
    ``try-restart`` so a profile that keeps it stopped is never
    force-started by a config save.
    """
    marker_changed = reconcile_hotspot_marker(current)
    prev_slice = _hotspot_slice(previous)
    cur_slice = _hotspot_slice(current)
    if marker_changed or prev_slice != cur_slice:
        _kick("ados-hostapd.service", "try-restart")
        _kick("ados-dnsmasq-gs.service", "reload-or-restart")
