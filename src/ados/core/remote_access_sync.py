"""Restart the remote-access tunnel unit on demand.

``remote_access.provider`` and ``remote_access.cloudflare.enabled`` say whether
this node is reachable beyond the LAN through an outbound tunnel run by a
separate systemd unit (``remote_access.cloudflare.service_name``,
``cloudflared`` by default). Enabling or disabling that unit when the posture
changes is a config side effect, run natively for every config writer.

What stays here is the one action a config write cannot express: restarting a
running tunnel so it picks up a freshly installed token. Only unit names that
start with ``cloudflared`` are acted on, because the name is a config value and
without the check a caller could restart an arbitrary unit.
"""

from __future__ import annotations

import re
import subprocess
from typing import Any

from ados.core.logging import get_logger

log = get_logger("remote_access_sync")

_SYSTEMCTL_TIMEOUT_S = 10.0
UNIT_NAME_RE = re.compile(r"^cloudflared[A-Za-z0-9@._-]*$")


def restart_tunnel_unit(unit: str) -> dict[str, Any]:
    """``systemctl --no-block try-restart`` the tunnel unit.

    ``try-restart`` restarts a running unit and never starts a stopped one, so
    a disabled tunnel stays down. Returns ``{"unit", "ok", "error"}``: ``ok``
    is true only when systemctl accepted the job. Never raises.
    """
    if not isinstance(unit, str) or not UNIT_NAME_RE.fullmatch(unit):
        log.warning("remote_access_unit_refused", unit=str(unit))
        return {"unit": unit, "ok": False, "error": "unit name refused"}
    try:
        result = subprocess.run(
            ["systemctl", "--no-block", "try-restart", unit],
            capture_output=True,
            timeout=_SYSTEMCTL_TIMEOUT_S,
            check=False,
        )
    except (OSError, subprocess.SubprocessError) as exc:
        log.warning("remote_access_unit_restart_failed", unit=unit, error=str(exc))
        return {"unit": unit, "ok": False, "error": str(exc)}
    if result.returncode != 0:
        error = result.stderr.decode("utf-8", errors="replace").strip() or (
            f"systemctl exited {result.returncode}"
        )
        log.warning("remote_access_unit_restart_failed", unit=unit, error=error)
        return {"unit": unit, "ok": False, "error": error}
    log.info("remote_access_unit_restarted", unit=unit)
    return {"unit": unit, "ok": True, "error": None}


__all__ = ["UNIT_NAME_RE", "restart_tunnel_unit"]
