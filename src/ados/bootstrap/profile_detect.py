"""Profile auto-detect for ADOS Drone Agent.

Runs a hardware fingerprint and picks `drone` or `ground_station`. If the
signals are ambiguous the result is `unconfigured` and a human picks via
OLED or the setup webapp.

Full design: product/specs/ados-ground-agent/04-profile-autodetect.md

This module has no hard runtime dependencies beyond the stdlib. `smbus2`
and `gpiozero` are used when available; otherwise we fall back to
shelling out to `i2cdetect` and to skipping the GPIO probe.

Run a dry-run from a shell:

    python -m ados.bootstrap.profile_detect
"""

from __future__ import annotations

import os
import re
import subprocess
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

try:
    import structlog

    log = structlog.get_logger()
except ImportError:  # pragma: no cover - fall back to stdlib logging
    import logging

    log = logging.getLogger(__name__)


# RTL8812 family USB IDs (vendor 0x0bda Realtek, known product IDs).
_RTL8812_IDS: set[tuple[int, int]] = {
    (0x0BDA, 0x8812),
    (0x0BDA, 0x881A),
    (0x0BDA, 0xA81A),
}

# Default I2C bus for OLED and default BCM GPIO pin set for the four buttons.
# HAL board profile (when available) can override both.
_DEFAULT_I2C_BUS = 1
_DEFAULT_BUTTON_GPIOS = [5, 6, 13, 19]

# Paths we probe for an FC serial link.
_MAVLINK_SERIAL_PATHS = [
    "/dev/ttyACM0",
    "/dev/ttyACM1",
    "/dev/ttyUSB0",
    "/dev/ttyUSB1",
    "/dev/serial0",
    "/dev/ttyAMA0",
]


# ---- Signal probes ---------------------------------------------------------


def probe_i2c_oled(bus: int = _DEFAULT_I2C_BUS) -> tuple[int, int, bool]:
    """Scan I2C bus for SSD1306 or SH1106 at 0x3C or 0x3D.

    Returns (ground_points, air_points, detected).
    """
    detected = False

    # Preferred path: smbus2 if already available.
    try:
        from smbus2 import SMBus  # type: ignore

        try:
            with SMBus(bus) as smb:
                for addr in (0x3C, 0x3D):
                    try:
                        smb.read_byte(addr)
                        detected = True
                        break
                    except OSError:
                        continue
        except (FileNotFoundError, PermissionError, OSError):
            detected = False
    except ImportError:
        # Fallback: shell out to i2cdetect.
        try:
            result = subprocess.run(
                ["i2cdetect", "-y", str(bus)],
                capture_output=True,
                text=True,
                timeout=3,
            )
            if result.returncode == 0:
                text = result.stdout.lower()
                # i2cdetect prints addresses as hex tokens in the grid.
                for addr in ("3c", "3d"):
                    if re.search(rf"\b{addr}\b", text):
                        detected = True
                        break
        except (FileNotFoundError, subprocess.TimeoutExpired, OSError):
            detected = False

    return (3, 0, detected) if detected else (0, 0, False)


def probe_gpio_buttons(
    pins: list[int] | None = None,
) -> tuple[int, int, bool]:
    """Confirm four GPIOs are idle-high with internal pull-up (buttons wired).

    Returns (ground_points, air_points, detected). If gpiozero is not
    available the probe is skipped and contributes zero points.
    """
    pins = pins or _DEFAULT_BUTTON_GPIOS

    try:
        from gpiozero import Button  # type: ignore
        from gpiozero.exc import BadPinFactory, GPIOZeroError  # type: ignore
    except ImportError:
        return 0, 0, False

    buttons: list[Any] = []
    all_idle_high = True
    try:
        for pin in pins:
            try:
                btn = Button(pin, pull_up=True)
                buttons.append(btn)
                # is_pressed is True when the pin reads LOW (pull-up idle is HIGH).
                if btn.is_pressed:
                    all_idle_high = False
            except (GPIOZeroError, BadPinFactory, OSError, ValueError):
                all_idle_high = False
                break
    finally:
        for btn in buttons:
            try:
                btn.close()
            except Exception:
                pass

    return (2, 0, True) if all_idle_high and len(buttons) == len(pins) else (0, 0, False)


def probe_rtl8812() -> tuple[int, int, bool]:
    """Scan lsusb for an RTL8812 family adapter."""
    try:
        result = subprocess.run(
            ["lsusb"],
            capture_output=True,
            text=True,
            timeout=3,
        )
        if result.returncode != 0:
            return 0, 0, False
        text = result.stdout.lower()
        for vid, pid in _RTL8812_IDS:
            needle = f"{vid:04x}:{pid:04x}"
            if needle in text:
                return 1, 1, True
    except (FileNotFoundError, subprocess.TimeoutExpired, OSError):
        pass
    return 0, 0, False


def probe_mavlink_serial() -> tuple[int, int, bool]:
    """Look for a plausible FC serial device.

    We do not read a heartbeat here. A present device earns +3 air points.
    Heartbeat confirmation is a separate probe done after the mavlink
    service is up (placeholder below).
    """
    for path in _MAVLINK_SERIAL_PATHS:
        if Path(path).exists():
            return 0, 3, True
    return 0, 0, False


def probe_gps_serial() -> tuple[int, int, bool]:
    """Placeholder: future GPS UART detection. Returns zero for now."""
    # TODO: enumerate /dev/ttyAMA* and /dev/serial* that are not claimed
    # by the FC and probe for NMEA or UBX frames.
    return 0, 0, False


def probe_fc_heartbeat() -> tuple[int, int, bool]:
    """Placeholder: real detection happens after the mavlink service starts."""
    # TODO: read the agent state IPC socket for a recent FC heartbeat.
    return 0, 0, False


def probe_uplink_type() -> tuple[int, int, bool]:
    """Check for an ethernet link or active USB gadget uplink."""
    eth_carrier = Path("/sys/class/net/eth0/carrier")
    usb_state = Path("/sys/class/net/usb0/operstate")

    detected = False
    try:
        if eth_carrier.is_file():
            if eth_carrier.read_text().strip() == "1":
                detected = True
    except OSError:
        pass

    if not detected:
        try:
            if usb_state.is_file():
                if usb_state.read_text().strip().lower() == "up":
                    detected = True
        except OSError:
            pass

    return (1, 0, True) if detected else (0, 0, False)


def probe_mesh_capable() -> bool:
    """True when a second wireless adapter is present beyond the primary WFB NIC.

    Mesh role (relay or receiver) requires one wireless NIC in monitor mode
    for WFB-ng plus a second NIC running 802.11s or IBSS for batman-adv.
    We approximate that by counting /sys/class/net/wlan* entries. The
    mesh_manager service validates driver support for 802.11s at service
    start; this probe is intentionally permissive so --with-mesh can still
    flag the node as mesh-capable at install time even if the second
    dongle is temporarily unplugged.

    The flag does not change the default role (which stays `direct`). It
    only controls whether the OLED shows the Mesh submenu and whether the
    GCS Hardware tab surfaces the Distributed RX and Mesh sub-views.
    """
    try:
        net_dir = Path("/sys/class/net")
        if not net_dir.is_dir():
            return False
        wlan_ifaces = [p.name for p in net_dir.iterdir() if p.name.startswith("wlan")]
        return len(wlan_ifaces) >= 2
    except OSError:
        return False


# ---- Decision -------------------------------------------------------------


def detect_profile(config_override: str | None = None) -> dict[str, Any]:
    """Run all probes and decide the profile.

    config_override: explicit value from /etc/ados/config.yaml. If it is
    "drone" or "ground_station" that value is returned without running
    any probes.
    """
    if config_override in ("drone", "ground_station"):
        return {
            "profile": config_override,
            "ground_score": 0,
            "air_score": 0,
            "signals": {"override": config_override},
            "detected_at": _now_iso(),
        }

    probes = {
        "oled_i2c": probe_i2c_oled(),
        "buttons_gpio": probe_gpio_buttons(),
        "rtl8812": probe_rtl8812(),
        "mavlink_serial": probe_mavlink_serial(),
        "gps_serial": probe_gps_serial(),
        "fc_heartbeat": probe_fc_heartbeat(),
        "uplink": probe_uplink_type(),
    }

    ground_score = sum(g for g, _a, _d in probes.values())
    air_score = sum(a for _g, a, _d in probes.values())

    if ground_score >= 4 and air_score <= 2:
        profile = "ground_station"
    elif air_score >= 4 and ground_score <= 2:
        profile = "drone"
    else:
        profile = "unconfigured"

    signals = {name: detected for name, (_g, _a, detected) in probes.items()}

    result = {
        "profile": profile,
        "ground_score": ground_score,
        "air_score": air_score,
        "signals": signals,
        # second wireless NIC gates mesh role visibility. Default False
        # so existing single-node deployments keep current UX.
        "mesh_capable": probe_mesh_capable(),
        "detected_at": _now_iso(),
    }
    try:
        log.info(
            "profile_detect_result",
            profile=profile,
            ground_score=ground_score,
            air_score=air_score,
        )
    except Exception:
        pass
    return result


def _now_iso() -> str:
    return (
        datetime.now(timezone.utc)
        .astimezone()
        .replace(microsecond=0)
        .isoformat()
    )


# ---- Persistence ----------------------------------------------------------


def write_profile_conf(
    result: dict[str, Any],
    path: str = "/etc/ados/profile.conf",
) -> bool:
    """Write the fingerprint snapshot to /etc/ados/profile.conf (YAML).

    Returns True on success. Failures are logged and swallowed because
    this runs early in boot when logging may not be wired yet.
    """
    try:
        import yaml  # local import so dry-run without pyyaml fails loudly
    except ImportError:
        try:
            log.error("profile_conf_write_failed", error="pyyaml not installed")
        except Exception:
            pass
        return False

    target = Path(path)
    try:
        target.parent.mkdir(parents=True, exist_ok=True)
        tmp = target.with_suffix(target.suffix + ".tmp")
        with open(tmp, "w") as f:
            yaml.safe_dump(result, f, sort_keys=True)
        os.replace(tmp, target)
        try:
            log.info("profile_conf_written", path=str(target))
        except Exception:
            pass
        return True
    except OSError as exc:
        try:
            log.error(
                "profile_conf_write_failed",
                path=str(target),
                error=str(exc),
            )
        except Exception:
            pass
        return False


# ---- Dry-run entry point --------------------------------------------------


def _main() -> None:
    """Print the detection result as YAML. No file is written."""
    result = detect_profile(config_override=None)
    try:
        import yaml

        print(yaml.safe_dump(result, sort_keys=True).rstrip())
    except ImportError:
        import json

        print(json.dumps(result, indent=2, sort_keys=True))


if __name__ == "__main__":
    _main()
