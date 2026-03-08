"""Cellular modem detection via ModemManager (mmcli)."""

from __future__ import annotations

import platform
import re
import subprocess
from dataclasses import dataclass

from ados.core.logging import get_logger

log = get_logger("hal.modem")


@dataclass
class ModemInfo:
    """Cellular modem status."""

    name: str
    operator: str
    signal_strength: int
    connection_state: str
    ip_address: str

    def to_dict(self) -> dict:
        return {
            "name": self.name,
            "operator": self.operator,
            "signal_strength": self.signal_strength,
            "connection_state": self.connection_state,
            "ip_address": self.ip_address,
        }


def _run_mmcli(args: list[str]) -> str | None:
    """Run mmcli with the given arguments and return stdout, or None on failure."""
    try:
        result = subprocess.run(
            ["mmcli", *args],
            capture_output=True,
            text=True,
            timeout=10,
        )
        if result.returncode == 0:
            return result.stdout
    except FileNotFoundError:
        log.debug("mmcli_not_found", msg="ModemManager not installed")
    except subprocess.TimeoutExpired:
        log.warning("mmcli_timeout", args=args)
    return None


def _extract_field(text: str, label: str) -> str:
    """Extract a named field value from mmcli output."""
    pattern = re.compile(rf"{re.escape(label)}\s*:\s*(.+)")
    match = pattern.search(text)
    if match:
        return match.group(1).strip()
    return ""


def _parse_modem_details(output: str) -> ModemInfo:
    """Parse mmcli -m N output into a ModemInfo."""
    name = _extract_field(output, "model")
    if not name:
        name = _extract_field(output, "manufacturer")
    if not name:
        name = "Unknown Modem"

    operator = _extract_field(output, "operator name")
    if not operator:
        operator = _extract_field(output, "operator-name")

    signal_str = _extract_field(output, "signal quality")
    signal_val = 0
    sig_match = re.search(r"(\d+)", signal_str)
    if sig_match:
        signal_val = int(sig_match.group(1))

    state = _extract_field(output, "state")
    if not state:
        state = "unknown"

    ip_address = _extract_field(output, "address")
    if not ip_address or ip_address == "--":
        ip_address = ""

    return ModemInfo(
        name=name,
        operator=operator,
        signal_strength=signal_val,
        connection_state=state,
        ip_address=ip_address,
    )


def get_modem_status(modem_index: int) -> ModemInfo | None:
    """Query a specific modem's status by index.

    Uses ``mmcli -m <index>`` to retrieve details.
    """
    if platform.system() != "Linux":
        log.debug("modem_skip_platform", platform=platform.system())
        return None

    output = _run_mmcli(["-m", str(modem_index)])
    if output is None:
        return None

    info = _parse_modem_details(output)
    log.info(
        "modem_status",
        index=modem_index,
        name=info.name,
        state=info.connection_state,
        signal=info.signal_strength,
    )
    return info


def detect_modem() -> ModemInfo | None:
    """Detect the first available cellular modem.

    Uses ``mmcli -L`` to list modems, then queries the first one found.
    Returns None on macOS and other non-Linux platforms.
    """
    if platform.system() != "Linux":
        log.debug("modem_skip_platform", platform=platform.system())
        return None

    output = _run_mmcli(["-L"])
    if output is None:
        return None

    # Parse modem list: lines like "/org/freedesktop/ModemManager1/Modem/0 ..."
    modem_pattern = re.compile(r"/Modem/(\d+)")
    match = modem_pattern.search(output)
    if not match:
        log.info("no_modem_found")
        return None

    modem_index = int(match.group(1))
    return get_modem_status(modem_index)
