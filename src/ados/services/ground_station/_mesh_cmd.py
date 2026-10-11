"""Subprocess and batctl helpers for the batman-adv mesh manager."""

from __future__ import annotations

import json
import subprocess

from ados.core.logging import get_logger
from ados.core.paths import MESH_GATEWAY_JSON

log = get_logger("ground_station.mesh_manager")


def _run(cmd: list[str], timeout: float = 10.0) -> tuple[int, str, str]:
    """Run a command with a hard timeout. Escalates TERM -> KILL.

    `subprocess.run(timeout=...)` calls `proc.kill()` internally on
    timeout but then waits for `communicate()` to drain stdout/stderr.
    If the process is deadlocked in the kernel (wedged WiFi driver,
    stuck `batctl`), that drain can itself hang. We use Popen directly
    so a final forced wait + resource release is bounded.
    """
    try:
        proc = subprocess.Popen(
            cmd,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
    except FileNotFoundError:
        return 127, "", "not found"

    try:
        stdout, stderr = proc.communicate(timeout=timeout)
        return proc.returncode, stdout, stderr
    except subprocess.TimeoutExpired:
        proc.terminate()
        try:
            stdout, stderr = proc.communicate(timeout=1.0)
        except subprocess.TimeoutExpired:
            proc.kill()
            try:
                stdout, stderr = proc.communicate(timeout=1.0)
            except subprocess.TimeoutExpired:
                # Kernel still holds the process. Give up and let the
                # zombie be reaped when the parent exits. We have
                # exhausted the recovery options without blocking the
                # caller further.
                return 124, "", "timeout (kill did not release)"
        return 124, stdout or "", stderr or "timeout"


def _modprobe_batman() -> bool:
    rc, _out, err = _run(["modprobe", "batman-adv"], timeout=10.0)
    if rc != 0:
        log.error("modprobe_batman_failed", err=err.strip())
        return False
    return True


_GATEWAY_PREFERENCE_PATH = MESH_GATEWAY_JSON


def _apply_persisted_gateway_preference() -> str | None:
    """Re-apply the operator's last gateway pin on mesh setup.

    Reads `/etc/ados/mesh/gateway.json` which is written by the REST
    endpoint when the operator clicks a pin button in the GCS.
    Returns the pinned MAC on success, or None if no preference is
    persisted or the file is unreadable.

    The file schema is `{"mode": "auto"|"pinned"|"off", "pinned_mac": str|null}`.
    Only the "pinned" mode with a non-null MAC triggers `batctl gw_sel`.
    Auto and off modes are already the default batman-adv behavior on
    fresh bringup; no explicit action needed here.
    """
    if not _GATEWAY_PREFERENCE_PATH.is_file():
        return None
    try:
        data = json.loads(_GATEWAY_PREFERENCE_PATH.read_text(encoding="utf-8"))
    except (OSError, ValueError) as exc:
        log.warning("gateway_preference_read_failed", error=str(exc))
        return None
    mode = data.get("mode")
    pinned_mac = data.get("pinned_mac")
    if mode != "pinned" or not pinned_mac:
        return None
    rc, _o, err = _run(["batctl", "gw_sel", str(pinned_mac)], timeout=5.0)
    if rc != 0:
        log.warning(
            "gateway_pin_apply_failed",
            mac=pinned_mac,
            err=err.strip(),
        )
        return None
    log.info("gateway_pin_applied", mac=pinned_mac)
    return str(pinned_mac)
