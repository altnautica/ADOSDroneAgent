"""Config-over-radio enable-marker reconcile + service kick.

The ``ados-tunnel-config`` systemd unit gates on ``/etc/ados/tunnel-enabled``
(``ConditionPathExists``), and the marker mirrors ``radio.tunnel.enabled`` — it
is never hand-managed. Two writers keep it true to the config:

* the installer (its systemd step reconciles the marker from the on-disk
  config on every install/upgrade), and
* the runtime config persist path (this module, called from the single
  ``/etc/ados/config.yaml`` write chokepoint), so enabling the channel through
  any config surface is sufficient — the marker lands and the unit is kicked
  with no manual ``systemctl``.

The kick uses ``systemctl --no-block reload-or-restart``: a running service
re-reads its config in place (its SIGHUP reload), a condition-skipped unit
re-evaluates the now-present marker and starts, and a disable leaves the
running service idling honestly (its own opt-in gate reads the fresh config)
until the next boot skips the unit entirely.

Everything here is best-effort: a marker or systemctl failure is logged and
never fails the config write that triggered it.
"""

from __future__ import annotations

import subprocess
from typing import Any

from ados.core.logging import get_logger

log = get_logger("tunnel_marker")

_SYSTEMCTL_TIMEOUT_S = 10.0


def _tunnel_slice(config: dict[str, Any] | None) -> dict[str, Any]:
    """The ``radio.tunnel`` mapping of a raw config dict (``{}`` when absent)."""
    if not isinstance(config, dict):
        return {}
    radio = config.get("radio")
    if not isinstance(radio, dict):
        return {}
    tunnel = radio.get("tunnel")
    return tunnel if isinstance(tunnel, dict) else {}


def reconcile_tunnel_marker(config: dict[str, Any] | None) -> bool:
    """Mirror ``radio.tunnel.enabled`` onto the enable marker.

    Returns True when the marker's presence CHANGED (used by callers that only
    act on a flip). Best-effort: an OSError is logged and reported as no-change
    so the caller's config write is never failed by the marker.
    """
    from ados.core.paths import TUNNEL_ENABLED_PATH

    enabled = bool(_tunnel_slice(config).get("enabled", False))
    try:
        exists = TUNNEL_ENABLED_PATH.exists()
        if enabled and not exists:
            TUNNEL_ENABLED_PATH.touch()
            log.info("tunnel_marker_written", path=str(TUNNEL_ENABLED_PATH))
            return True
        if not enabled and exists:
            TUNNEL_ENABLED_PATH.unlink()
            log.info("tunnel_marker_removed", path=str(TUNNEL_ENABLED_PATH))
            return True
    except OSError as exc:
        log.warning("tunnel_marker_reconcile_failed", error=str(exc))
    return False


def _kick_tunnel_service() -> None:
    """Fire-and-forget ``reload-or-restart`` of the config-tunnel unit.

    ``--no-block`` queues the job without waiting, so a config-write request is
    never held on systemd; failures (no systemctl on a dev host, unit not
    installed) are logged at debug and swallowed.
    """
    try:
        subprocess.run(
            ["systemctl", "--no-block", "reload-or-restart", "ados-tunnel-config.service"],
            capture_output=True,
            timeout=_SYSTEMCTL_TIMEOUT_S,
            check=False,
        )
    except (OSError, subprocess.SubprocessError) as exc:
        log.debug("tunnel_service_kick_failed", error=str(exc))


def sync_after_config_write(
    previous: dict[str, Any] | None, current: dict[str, Any] | None
) -> None:
    """Reconcile the marker + kick the unit after a config write.

    ``previous`` is the config as it was on disk before the write (``None`` when
    unknown/absent), ``current`` the just-persisted dict. The marker is
    reconciled on every write (idempotent, self-healing); the unit is kicked
    only when the ``radio.tunnel`` slice actually changed (so a running service
    re-reads a flipped ``command_enabled`` in place, not only on an
    enable/disable flip), so unrelated config saves never churn the service.
    Best-effort throughout.
    """
    marker_changed = reconcile_tunnel_marker(current)
    prev_slice = _tunnel_slice(previous)
    cur_slice = _tunnel_slice(current)
    if marker_changed or prev_slice != cur_slice:
        _kick_tunnel_service()
