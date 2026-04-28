"""Failover policy: priority chain, hysteresis, persistence, and the
kernel routing-table primitive.

The router orchestrator owns the asyncio loop and the live state. This
module owns the pure logic: the configured priority list (load/save),
the hysteresis thresholds, the routing-table replace command, and the
selection helpers that pick the next viable uplink.
"""

from __future__ import annotations

import json
import os
import subprocess
from pathlib import Path
from typing import Optional

import structlog

__all__ = [
    "DEFAULT_PRIORITY",
    "PRIORITY_METRIC",
    "FAIL_DOWN_THRESHOLD",
    "SUCCESS_UP_THRESHOLD",
    "SWITCH_COOLDOWN_SECONDS",
    "load_priority",
    "save_priority",
    "validate_priority",
    "select_failover_target",
    "select_higher_priority",
    "apply_default_route",
]

log = structlog.get_logger(__name__)

# Default priority chain. `wlan0_ap` is the LAN-side SSID served to
# phones and laptops, not an uplink, so it is absent here.
DEFAULT_PRIORITY: list[str] = ["eth0", "wlan0_client", "wwan0", "usb0"]

# Per-uplink route metric. Lower number wins in the kernel routing
# table, so we keep the gap large to survive manual `ip route` probes.
PRIORITY_METRIC = {
    "eth0": 100,
    "wlan0_client": 200,
    "wwan0": 300,
    "usb0": 400,
}

# Hysteresis knobs. Three consecutive fails flip us down to the next
# viable uplink. Three consecutive successes on a higher-priority
# uplink flip us back up. A 30 second cooldown between switches
# prevents thrash when two uplinks are both flaky.
FAIL_DOWN_THRESHOLD = 3
SUCCESS_UP_THRESHOLD = 3
SWITCH_COOLDOWN_SECONDS = 30.0


def load_priority(path: Path) -> list[str]:
    """Load the priority list from disk, falling back to the default."""
    try:
        if path.exists():
            raw = json.loads(path.read_text(encoding="utf-8"))
            order = raw.get("priority")
            if isinstance(order, list) and all(isinstance(x, str) for x in order):
                if order:
                    return order
    except (OSError, ValueError) as exc:
        log.warning("uplink.priority_load_failed", error=str(exc))
    return list(DEFAULT_PRIORITY)


def save_priority(path: Path, priority: list[str]) -> None:
    """Atomically persist the priority list to disk."""
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        tmp = path.with_suffix(".json.tmp")
        tmp.write_text(json.dumps({"priority": priority}), encoding="utf-8")
        os.replace(tmp, path)
    except OSError as exc:
        log.warning("uplink.priority_save_failed", error=str(exc))


def validate_priority(priority_list: list[str]) -> None:
    """Raise ValueError if the priority list is empty or non-string."""
    if not priority_list or not all(isinstance(x, str) for x in priority_list):
        raise ValueError("priority must be a non-empty list of strings")


def select_failover_target(
    priority: list[str],
    available: list[str],
    current: Optional[str],
) -> Optional[str]:
    """Pick the next viable uplink below the current one.

    First tries strictly lower-priority entries below the current
    uplink. If none are available, falls back to any available uplink
    that is not the current one. Returns None when the current uplink
    is the only available option.
    """
    if current in priority:
        current_idx = priority.index(current)
        for candidate in priority[current_idx + 1:]:
            if candidate in available:
                return candidate
    alternatives = [u for u in available if u != current]
    if alternatives:
        return alternatives[0]
    return None


def select_higher_priority(
    priority: list[str],
    available: list[str],
    current: Optional[str],
) -> list[str]:
    """Return available uplinks ranked above the current one."""
    if current is None or current not in priority:
        return []
    current_idx = priority.index(current)
    return [
        u for u in available
        if u in priority and priority.index(u) < current_idx
    ]


def apply_default_route(iface: str, gateway: Optional[str]) -> bool:
    """Replace the kernel default route to point at `iface`."""
    metric = PRIORITY_METRIC.get(iface, 500)
    cmd: list[str]
    if gateway:
        cmd = [
            "ip", "route", "replace", "default",
            "via", gateway, "dev", iface,
            "metric", str(metric),
        ]
    else:
        cmd = [
            "ip", "route", "replace", "default",
            "dev", iface, "metric", str(metric),
        ]
    try:
        result = subprocess.run(
            cmd, check=False, capture_output=True, timeout=5
        )
        if result.returncode != 0:
            log.warning(
                "uplink.route_replace_failed",
                cmd=" ".join(cmd),
                rc=result.returncode,
                stderr=result.stderr.decode(errors="replace").strip(),
            )
            return False
        log.info(
            "uplink.route_applied",
            iface=iface,
            gateway=gateway,
            metric=metric,
        )
        return True
    except (OSError, subprocess.SubprocessError) as exc:
        log.warning("uplink.route_apply_exc", error=str(exc))
        return False
