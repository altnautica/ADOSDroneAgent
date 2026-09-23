"""Start or stop the remote-access tunnel unit when its config changes.

``remote_access.provider`` and ``remote_access.cloudflare.enabled`` say whether
this node is reachable beyond the LAN through an outbound tunnel. The tunnel
itself is a separate systemd unit (``remote_access.cloudflare.service_name``,
``cloudflared`` by default), so a config write alone changes nothing: an
operator who turns the tunnel off would see the change saved while the node
stayed publicly reachable.

This module is called from the single ``/etc/ados/config.yaml`` write
chokepoint. When the effective tunnel posture changes it enables and starts the
unit (``provider == "cloudflare"`` and ``enabled``), or disables and stops it
(anything else), so the unit also stays in that state across a reboot. The
node's own status report (``remote_access.status`` in the setup status) reads
the unit's state back, so a surface can show whether the start or stop landed.

Only unit names that start with ``cloudflared`` are acted on. The service name
is a config value, and without the check a config write could stop an
arbitrary unit.

Best-effort: a systemctl failure is logged and never fails the config write
that triggered it.
"""

from __future__ import annotations

import re
import subprocess
from typing import Any

from ados.core.logging import get_logger

log = get_logger("remote_access_sync")

_SYSTEMCTL_TIMEOUT_S = 10.0
_DEFAULT_UNIT = "cloudflared"
_UNIT_NAME = re.compile(r"^cloudflared[A-Za-z0-9@._-]*$")


def _posture(config: dict[str, Any] | None) -> tuple[bool, str]:
    """``(tunnel wanted, unit name)`` for a raw config mapping.

    An absent block reads as the packaged default: provider ``none``, tunnel
    off, unit ``cloudflared``.
    """
    remote = config.get("remote_access") if isinstance(config, dict) else None
    if not isinstance(remote, dict):
        return False, _DEFAULT_UNIT
    cf = remote.get("cloudflare")
    if not isinstance(cf, dict):
        cf = {}
    unit = cf.get("service_name")
    unit = unit.strip() if isinstance(unit, str) and unit.strip() else _DEFAULT_UNIT
    wanted = remote.get("provider") == "cloudflare" and cf.get("enabled") is True
    return wanted, unit


def _kick(unit: str, wanted: bool) -> None:
    """``systemctl enable --now`` or ``disable --now`` the tunnel unit.

    ``--no-block`` queues the start or stop without holding the config-write
    request on systemd.
    """
    if not _UNIT_NAME.match(unit):
        log.warning("remote_access_unit_refused", unit=unit)
        return
    verb = "enable" if wanted else "disable"
    try:
        result = subprocess.run(
            ["systemctl", "--no-block", verb, "--now", unit],
            capture_output=True,
            timeout=_SYSTEMCTL_TIMEOUT_S,
            check=False,
        )
    except (OSError, subprocess.SubprocessError) as exc:
        log.warning("remote_access_unit_kick_failed", unit=unit, verb=verb, error=str(exc))
        return
    if result.returncode != 0:
        log.warning(
            "remote_access_unit_kick_failed",
            unit=unit,
            verb=verb,
            error=result.stderr.decode("utf-8", errors="replace").strip(),
        )
    else:
        log.info("remote_access_unit_kicked", unit=unit, verb=verb)


def sync_after_config_write(
    previous: dict[str, Any] | None, current: dict[str, Any] | None
) -> None:
    """Bring the tunnel unit in line with a just-written config.

    Acts only when the tunnel posture or the unit name changed, so unrelated
    config saves never restart a running tunnel. A renamed unit stops the old
    one before the new one starts.
    """
    before = _posture(previous)
    after = _posture(current)
    if before == after:
        return
    was_wanted, old_unit = before
    wanted, unit = after
    if was_wanted and old_unit != unit:
        _kick(old_unit, False)
    _kick(unit, wanted)


__all__ = ["sync_after_config_write"]
