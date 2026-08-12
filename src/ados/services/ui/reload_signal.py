"""SIGHUP signaling helper for ground-station UI services.

The ground-station OLED runs as its own systemd unit (`ados-oled.service`);
the button mapping is owned by the native arbiter (`ados-pic.service`), which
reads the front-panel GPIO in-process. When the GCS or captive
portal pushes a UI config update via REST (`PUT /ui/oled`, `PUT /ui/buttons`,
`PUT /ui/screens`), the REST handler must tell each service to reload its
mapping from `ADOSConfig.ground_station.ui` without restarting the unit.

Strategy: SIGHUP. The services install an asyncio SIGHUP handler that
reloads config and rebuilds whatever cached state depends on it (button
action map, screen list). We resolve the target PID via `systemctl show
-p MainPID --value <unit>`, which works the same on Debian, Ubuntu, and
Radxa BSP. If the unit is inactive or systemd is unavailable (dev tree,
non-systemd container) we degrade silently and log a debug line.

This helper is intentionally synchronous and dependency-free so it can be
called from inside a FastAPI request handler without an event loop hop.
"""

from __future__ import annotations

import os
import signal
import subprocess

from ados.core.logging import get_logger

log = get_logger("ui.reload_signal")


def _resolve_main_pid(unit_name: str) -> int | None:
    """Return the MainPID of `unit_name` or None when not running.

    `systemctl show -p MainPID --value` prints `0` when the unit is
    inactive or unknown. Anything else is the live PID.
    """
    try:
        out = subprocess.run(
            ["systemctl", "show", "-p", "MainPID", "--value", unit_name],
            check=False,
            capture_output=True,
            text=True,
            timeout=2.0,
        )
    except (OSError, subprocess.SubprocessError) as exc:
        log.debug("systemctl_show_failed", unit=unit_name, error=str(exc))
        return None

    raw = (out.stdout or "").strip()
    if not raw or raw == "0":
        return None
    try:
        pid = int(raw)
    except ValueError:
        return None
    return pid if pid > 0 else None


def signal_sighup(unit_name: str) -> bool:
    """Send SIGHUP to the MainPID of `unit_name`. Best-effort.

    Returns True when a signal was actually delivered. Returns False when
    the unit is inactive, the PID lookup failed, or `os.kill` raised. The
    caller logs a higher-level success line on True.
    """
    pid = _resolve_main_pid(unit_name)
    if pid is None:
        log.debug("sighup_skipped_no_pid", unit=unit_name)
        return False
    try:
        os.kill(pid, signal.SIGHUP)
        log.info("sighup_delivered", unit=unit_name, pid=pid)
        return True
    except (OSError, ProcessLookupError) as exc:
        log.debug("sighup_failed", unit=unit_name, pid=pid, error=str(exc))
        return False


def signal_oled_reload() -> bool:
    """SIGHUP the OLED service so it reloads screen order and brightness."""
    return signal_sighup("ados-oled.service")


def signal_buttons_reload() -> bool:
    """SIGHUP the daemon that owns the button mapping so a write takes effect.

    That is ``ados-pic``: the native arbiter reads the front-panel GPIO
    in-process and rebuilds its mapping on SIGHUP. The packaged button service
    it replaced is gone, so signalling that unit left the mapping unchanged
    until the daemon next restarted.
    """
    return signal_sighup("ados-pic.service")
