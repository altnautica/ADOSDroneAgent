"""Profile auto-detect for ADOS Drone Agent.

Runs a hardware fingerprint and always returns a usable profile (`drone`
or `ground_station`). The decision tail is a strict argmax on the live
probes, with a stable tiebreaker on the last persisted profile and a
final `drone` default so a fresh-flashed board with no signals still
boots into a known state.

This module has no hard runtime dependencies beyond the stdlib. `smbus2`,
`gpiozero`, and `pyserial` are used when available; otherwise the
matching probes silently contribute zero points.

Run a dry-run from a shell:

    python -m ados.bootstrap.profile_detect
"""

from __future__ import annotations

import json
import os
import re
import socket
import subprocess
import time
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Callable

try:
    import structlog

    log = structlog.get_logger()
except ImportError:  # pragma: no cover - fall back to stdlib logging
    import logging

    log = logging.getLogger(__name__)

from ados.core.paths import PROFILE_CONF


# RTL8812 family USB IDs used on WFB-ng primary adapters.
# (vendor 0x0bda Realtek, known product IDs for the RTL8812 series).
# 0xB812 is the RTL8822E-class silicon used by the LB-LINK BL-M8812EU2
# module and similar vendor rebrands of the RTL8812EU.
_RTL8812_IDS: set[tuple[int, int]] = {
    (0x0BDA, 0x8812),
    (0x0BDA, 0x881A),
    (0x0BDA, 0x881B),
    (0x0BDA, 0x881C),
    (0x0BDA, 0xB812),
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

# Known FC USB vendor IDs. Match grants a strong "this is a drone"
# signal. A bare USB-serial adapter without an FC behind it would not
# match any of these, so the score boost is safe to add on top of the
# port-exists baseline.
#
# - 0x1209: pid.codes umbrella vendor used by SpeedyBee, ArduPilot
#   configurable bootloaders, and a long tail of community FC vendors.
# - 0x0483: STMicroelectronics — generic STM32 virtual com port, used
#   by raw STM32 dev boards. Less specific than 1209 so we still match
#   it but rely on the port-exists baseline already covering it.
# - 0x1d50: OpenMoko — OpenPilot, LibrePilot Revolution boards.
# - 0x26ac: 3D Robotics — original Pixhawk hardware.
# - 0x2dae: Hex / ProfiCNC — Cube series autopilots.
# - 0x3162: Holybro — current Pixhawk variants and Durandal.
# - 0x35a4: mRobotics — mRo Pixracer / Control Zero.
_FC_USB_VENDOR_IDS: set[int] = {
    0x1209,
    0x0483,
    0x1D50,
    0x26AC,
    0x2DAE,
    0x3162,
    0x35A4,
}


def _read_usb_vendor_for_tty(tty_path: str) -> int | None:
    """Read the USB vendor ID of the device backing ``tty_path``.

    Resolves ``/sys/class/tty/<name>/device`` (the USB interface
    symlink), walks up one level to the USB device, and reads
    ``idVendor`` as hex. Returns ``None`` for non-USB ttys (e.g.
    ``/dev/ttyAMA0`` which is a UART on the SoC, not a USB device).
    Failures are swallowed so the caller can fall back to the
    port-exists baseline.
    """
    name = Path(tty_path).name
    link = Path(f"/sys/class/tty/{name}/device")
    if not link.exists():
        return None
    try:
        iface_real = link.resolve(strict=True)
    except (OSError, RuntimeError):
        return None
    # USB tty: .../<bus>-<port>/<bus>-<port>:<config>.<iface>
    # parent of the interface is the USB device which carries idVendor.
    usb_device = iface_real.parent
    id_file = usb_device / "idVendor"
    if not id_file.is_file():
        return None
    try:
        raw = id_file.read_text().strip()
    except OSError:
        return None
    try:
        return int(raw, 16)
    except ValueError:
        return None


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

    A present device earns +3 air points (baseline). When the backing
    USB device's vendor ID matches a known autopilot vendor
    (pid.codes, ST, OpenMoko, 3DR, Hex, Holybro, mRo) we add +3 more
    so a real FC outweighs the hostname tiebreaker even on hybrid
    bench rigs where the box also has a WFB-ng dongle plugged in.

    Heartbeat confirmation lives in ``probe_fc_heartbeat`` and runs
    against ``/run/ados/state.sock`` once the mavlink service is up.
    Install-time detection runs before that socket exists, so the
    VID match is what closes the install-time gap.
    """
    for path in _MAVLINK_SERIAL_PATHS:
        if not Path(path).exists():
            continue
        vid = _read_usb_vendor_for_tty(path)
        if vid is not None and vid in _FC_USB_VENDOR_IDS:
            return 0, 6, True
        return 0, 3, True
    return 0, 0, False


_GPS_CANDIDATE_PATHS = [
    "/dev/ttyAMA0",
    "/dev/ttyAMA1",
    "/dev/ttyAMA2",
    "/dev/ttyS0",
    "/dev/ttyS1",
    "/dev/ttyS2",
    "/dev/serial1",
    "/dev/serial2",
]

_NMEA_PREFIXES = (b"$GP", b"$GN", b"$GL", b"$GA", b"$GB")
_UBX_SYNC = b"\xb5\x62"


def probe_gps_serial(timeout: float = 2.0) -> tuple[int, int, bool]:
    """Sample candidate UARTs for an NMEA or UBX frame.

    Walks plausible serial devices that are not already in use as the
    primary FC link, opens each at 9600 baud, samples for up to a few
    hundred milliseconds, and returns on the first match. A receiver
    that is talking at a non-default baud will not match here, which
    is acceptable: the probe is one signal in a seven-signal vote, not
    a full GPS configurator.
    """
    try:
        import serial  # type: ignore
    except ImportError:
        return 0, 0, False

    fc_paths = {p for p in _MAVLINK_SERIAL_PATHS if Path(p).exists()}
    candidates = [
        p for p in _GPS_CANDIDATE_PATHS if Path(p).exists() and p not in fc_paths
    ]
    if not candidates:
        return 0, 0, False

    per_port_timeout = max(timeout / len(candidates), 0.15)

    for path in candidates:
        try:
            with serial.Serial(
                path,
                baudrate=9600,
                timeout=per_port_timeout,
                exclusive=True,
            ) as port:
                buf = port.read(256)
        except (OSError, ValueError, serial.SerialException):  # type: ignore[attr-defined]
            continue
        if not buf:
            continue
        if any(buf.find(prefix) >= 0 for prefix in _NMEA_PREFIXES):
            return 0, 3, True
        if buf.find(_UBX_SYNC) >= 0:
            return 0, 3, True

    return 0, 0, False


def probe_fc_heartbeat(timeout: float = 1.5) -> tuple[int, int, bool]:
    """Read the agent state socket for a live FC heartbeat.

    The mavlink service publishes a 10 Hz state snapshot to the unix
    socket at `/run/ados/state.sock`. The first line on connect is the
    most recent snapshot, so a single short read is enough to tell us
    whether the FC is connected. On a brand-new boot the socket may
    not exist yet; that's expected and the probe returns zero so the
    decision falls back to the persistence tiebreaker.
    """
    state_sock = "/run/ados/state.sock"
    if not Path(state_sock).exists():
        return 0, 0, False

    deadline = time.monotonic() + timeout
    buf = bytearray()
    try:
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            sock.settimeout(timeout)
            sock.connect(state_sock)
            while time.monotonic() < deadline and b"\n" not in buf:
                remaining = max(0.05, deadline - time.monotonic())
                sock.settimeout(remaining)
                chunk = sock.recv(4096)
                if not chunk:
                    break
                buf.extend(chunk)
        finally:
            try:
                sock.close()
            except OSError:
                pass
    except (OSError, socket.timeout):
        return 0, 0, False

    line, _, _ = bytes(buf).partition(b"\n")
    if not line:
        return 0, 0, False
    try:
        data = json.loads(line)
    except json.JSONDecodeError:
        return 0, 0, False

    if isinstance(data, dict) and bool(data.get("fc_connected")):
        return 0, 3, True
    return 0, 0, False


def probe_uplink_type() -> tuple[int, int, bool]:
    """Check for any active uplink: ethernet, USB tether, or WiFi.

    Returns (ground_score, air_score, detected). The score columns
    are unchanged from the original scoring contract; only the third
    boolean is consumed by the hardware-check uplink probe, and it
    now also flips to True when wlan* is up.
    """
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

    if not detected:
        try:
            for wlan_dir in sorted(Path("/sys/class/net").glob("wlan*")):
                op = wlan_dir / "operstate"
                if op.is_file() and op.read_text().strip().lower() == "up":
                    detected = True
                    break
        except OSError:
            pass

    return (1, 0, True) if detected else (0, 0, False)


def probe_uplink_kinds() -> list[str]:
    """Return a list of active uplink kinds: ``ethernet``, ``usb-tether``, ``wifi``.

    Empty list when nothing is up. Used by the hardware-check uplink
    probe so it can show ``Active via WiFi`` instead of a generic
    ``ethernet/USB`` blob.
    """
    kinds: list[str] = []
    try:
        eth = Path("/sys/class/net/eth0/carrier")
        if eth.is_file() and eth.read_text().strip() == "1":
            kinds.append("ethernet")
    except OSError:
        pass
    try:
        usb = Path("/sys/class/net/usb0/operstate")
        if usb.is_file() and usb.read_text().strip().lower() == "up":
            kinds.append("USB tether")
    except OSError:
        pass
    try:
        for wlan_dir in sorted(Path("/sys/class/net").glob("wlan*")):
            op = wlan_dir / "operstate"
            if op.is_file() and op.read_text().strip().lower() == "up":
                kinds.append("WiFi")
                break
    except OSError:
        pass
    return kinds


def probe_mesh_capable() -> bool:
    """True when a second USB-attached wireless adapter is present.

    Mesh role (relay or receiver) requires one wireless NIC in monitor mode
    for WFB-ng plus a second NIC running 802.11s or IBSS for batman-adv.
    The second NIC must be USB-attached so it can be hot-plugged and does
    not clash with the SBC's onboard SDIO or PCIe wireless (e.g. Pi 4B's
    Broadcom module or a Radxa CM4 onboard chip). Counting raw
    /sys/class/net/wlan* entries is not enough because onboard WiFi shows
    up there too; we walk each wlan*/device symlink and match the bus
    type before counting.

    The flag does not change the default role (which stays `direct`). It
    only controls whether the OLED shows the Mesh submenu and whether the
    GCS Hardware tab surfaces the Distributed RX and Mesh sub-views.
    """
    try:
        net_dir = Path("/sys/class/net")
        if not net_dir.is_dir():
            return False
        usb_wlan_count = 0
        for iface in net_dir.iterdir():
            if not iface.name.startswith("wlan"):
                continue
            device_link = iface / "device"
            if not device_link.exists():
                continue
            try:
                resolved = os.path.realpath(device_link)
            except OSError:
                continue
            # Device path contains "/usb" only when the NIC is attached via
            # USB. Onboard SDIO shows "/sdio/", PCIe shows "/pci/", etc.
            if "/usb" in resolved:
                usb_wlan_count += 1
        return usb_wlan_count >= 2
    except OSError:
        return False


# ---- Decision -------------------------------------------------------------


def _hostname_suggested_profile() -> str | None:
    """Best-effort hostname → profile heuristic for the tiebreaker.

    Returns ``"drone"`` / ``"ground_station"`` when the system hostname
    matches a well-known prefix, ``None`` otherwise. Mirrors (but is
    independent of) the same helper in ``ados.setup.profile`` — kept
    private here so the bootstrap module never imports the setup tree.
    """
    try:
        name = (socket.gethostname() or "").strip().lower()
    except OSError:
        return None
    if not name:
        return None
    if name.startswith(("groundnode", "groundstation", "gcs", "gs-")):
        return "ground_station"
    if name.startswith(("skynode", "drone", "rig-", "uav")):
        return "drone"
    return None


def detect_profile(config_override: str | None = None) -> dict[str, Any]:
    """Run all probes and decide the profile.

    Always returns a usable profile (`drone` or `ground_station`). The
    decision tail is a strict argmax on the live probes, with a stable
    tiebreaker on the last persisted profile and a final `drone`
    default for a fresh-flashed board with no signals. The result
    includes a `source` field marking which branch of the decision
    produced the profile so the setup status can surface "auto" vs
    "needs review" cleanly.

    config_override: explicit value from /etc/ados/config.yaml. If it
    is "drone" or "ground_station" that value is returned without
    running any probes.
    """
    if config_override in ("drone", "ground_station"):
        return {
            "profile": config_override,
            "source": "override",
            "ground_score": 0,
            "air_score": 0,
            "signals": {"override": config_override},
            "mesh_capable": False,
            "detected_at": _now_iso(),
        }

    # Probes are independent. The slow ones shell out to i2cdetect or
    # lsusb with a 3 s timeout; on a freshly hot-plugged USB bus those
    # calls sit waiting. Running them in a thread pool drops worst-case
    # detection time from roughly the sum of timeouts to roughly the
    # slowest single probe.
    probe_callables: dict[str, Callable[[], tuple[int, int, bool]]] = {
        "oled_i2c": probe_i2c_oled,
        "buttons_gpio": probe_gpio_buttons,
        "rtl8812": probe_rtl8812,
        "mavlink_serial": probe_mavlink_serial,
        "gps_serial": probe_gps_serial,
        "fc_heartbeat": probe_fc_heartbeat,
        "uplink": probe_uplink_type,
    }

    probes: dict[str, tuple[int, int, bool]] = {}
    with ThreadPoolExecutor(max_workers=len(probe_callables)) as pool:
        futures = {name: pool.submit(fn) for name, fn in probe_callables.items()}
        for name, future in futures.items():
            try:
                probes[name] = future.result()
            except Exception:
                probes[name] = (0, 0, False)

    ground_score = sum(g for g, _a, _d in probes.values())
    air_score = sum(a for _g, a, _d in probes.values())

    # Hostname is operator-controlled but easy to leave stale: a box
    # named `groundnode` six months ago might be wired up as a drone
    # today. Treat it as a soft tiebreaker rather than a hardware
    # override, so a real FC with a matched USB vendor (6 air points)
    # still wins over an inherited "groundnode" hostname. Operators
    # who want hard pinning use `agent.profile` in /etc/ados/config.yaml
    # or `ados profile set`.
    hostname_pick = _hostname_suggested_profile()
    HOSTNAME_WEIGHT = 2
    hostname_source = ""
    if hostname_pick == "ground_station":
        ground_score += HOSTNAME_WEIGHT
        hostname_source = "hostname"
    elif hostname_pick == "drone":
        air_score += HOSTNAME_WEIGHT
        hostname_source = "hostname"

    if air_score > ground_score:
        profile = "drone"
        source = hostname_source or "detected"
    elif ground_score > air_score:
        profile = "ground_station"
        source = hostname_source or "detected"
    else:
        # Tied even after hostname weighting (hostname carried no
        # signal). Fall back to the last-known persisted profile, then
        # the safe default.
        prior = _read_last_known_profile()
        if prior in ("drone", "ground_station"):
            profile = prior
            source = "tiebreaker"
        else:
            profile = "drone"
            source = "default"

    signals = {name: detected for name, (_g, _a, detected) in probes.items()}

    result = {
        "profile": profile,
        "source": source,
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
            source=source,
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


def _read_last_known_profile(path: str = str(PROFILE_CONF)) -> str | None:
    """Return the profile recorded in profile.conf, or None on any error.

    Used as a tiebreaker when the live probes are exactly tied. Failures
    (file missing, yaml unavailable, parse error) collapse to None so
    the caller falls through to the "drone" default and the operator can
    review the auto-pick from the dashboard.
    """
    target = Path(path)
    if not target.is_file():
        return None
    try:
        import yaml
    except ImportError:
        return None
    try:
        with open(target) as f:
            data = yaml.safe_load(f) or {}
    except (OSError, yaml.YAMLError):
        return None
    value = data.get("profile") if isinstance(data, dict) else None
    if value in ("drone", "ground_station"):
        return value
    return None


def write_profile_conf(
    result: dict[str, Any],
    path: str = str(PROFILE_CONF),
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
