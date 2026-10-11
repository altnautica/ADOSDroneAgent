"""Hardware signal probes for the boot-time profile auto-detect.

Each probe returns ``(ground_points, air_points, detected)``. Re-exported
from ``ados.bootstrap.profile_detect``.
"""

from __future__ import annotations

import os
import re
import socket
import subprocess
import time
from pathlib import Path
from typing import Any

# RTL8812 family USB IDs used on WFB-ng primary adapters.
# (vendor 0x0bda Realtek, known product IDs for the RTL8812 series).
# 0xB812 is the RTL8822E-class silicon used by the LB-LINK BL-M8812EU2
# module and similar vendor rebrands of the RTL8812EU. 0xA81A is the
# product ID some vendors expose on RTL8811AU / RTL8814AU rebrands
# that ship with the same monitor-mode driver pack; observed live on
# the Realtek "802.11ac NIC" silicon shipped with several ground
# station radio kits and confirmed compatible with the same DKMS
# driver tree.
_RTL8812_IDS: set[tuple[int, int]] = {
    (0x0BDA, 0x8812),
    (0x0BDA, 0x881A),
    (0x0BDA, 0x881B),
    (0x0BDA, 0x881C),
    (0x0BDA, 0xA81A),
    (0x0BDA, 0xB812),
}

# Default I2C bus for the OLED.
_DEFAULT_I2C_BUS = 1

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


def board_button_pins() -> list[int]:
    """BCM pins the matched HAL board profile declares as front-panel buttons.

    Empty when the board declares none or the HAL is unavailable.
    """
    try:
        from ados.hal.detect import detect_board_profile

        profile = detect_board_profile()
    except Exception:
        return []
    if profile is None:
        return []
    return [button.pin for button in profile.gpio_buttons]


def probe_gpio_buttons() -> tuple[int, int, bool]:
    """Report whether the board declares front-panel buttons.

    Returns (ground_points, air_points, detected). Wiring cannot be read
    back: an unconnected GPIO with a pull-up reads the same as an idle
    button, and the native input service holds the lines once it runs. A
    declaration is therefore reported but earns no profile points.
    """
    return 0, 0, bool(board_button_pins())


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
        if vid is None:
            # An onboard SoC UART (serial0/ttyAMA0) exists on every Pi
            # whether or not an FC is wired to it; without a heartbeat it is
            # no evidence of a drone.
            continue
        if vid in _FC_USB_VENDOR_IDS:
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


def _config_claimed_serial_paths() -> set[str]:
    """Resolved device paths the node config assigns to a service.

    Raises when the config cannot be read, so the caller can refuse to probe
    rather than guess which ports are free.
    """
    from ados.core.config import load_config

    config = load_config()
    claimed: set[str] = set()
    mavlink_port = str(config.mavlink.serial_port or "")
    if mavlink_port.startswith("/"):
        claimed.add(os.path.realpath(mavlink_port))
    crsf_device = config.radio.crsf.device
    if crsf_device:
        claimed.add(os.path.realpath(crsf_device))
    return claimed


def _serial_paths_held_open(paths: set[str], proc_root: str = "/proc") -> set[str] | None:
    """Subset of resolved ``paths`` some other process has open.

    Returns None when the holders cannot be established (no procfs, or a
    process whose descriptors this caller may not read).
    """
    proc = Path(proc_root)
    if not proc.is_dir():
        return None
    own_pid = str(os.getpid())
    held: set[str] = set()
    for entry in proc.iterdir():
        if not entry.name.isdigit() or entry.name == own_pid:
            continue
        try:
            fds = list((entry / "fd").iterdir())
        except PermissionError:
            return None
        except OSError:
            # The process exited between listing and reading.
            continue
        for fd in fds:
            try:
                target = os.readlink(fd)
            except OSError:
                continue
            if target in paths:
                held.add(target)
    return held


def probe_gps_serial(timeout: float = 2.0) -> tuple[int, int, bool]:
    """Sample free candidate UARTs for an NMEA or UBX frame.

    Opening a port reprograms its line speed, so a port is sampled only
    when nothing can be using it: it is not a flight-controller path, the
    config does not assign it to a service, and no other process has it
    open. When that cannot be established the probe samples nothing. A free
    port is opened at 9600 baud; a receiver talking at another baud will not
    match, which is acceptable for one signal in the profile vote.
    """
    try:
        import serial  # type: ignore
    except ImportError:
        return 0, 0, False

    candidates = {
        os.path.realpath(p) for p in _GPS_CANDIDATE_PATHS if Path(p).exists()
    }
    candidates -= {
        os.path.realpath(p) for p in _MAVLINK_SERIAL_PATHS if Path(p).exists()
    }
    if not candidates:
        return 0, 0, False
    try:
        candidates -= _config_claimed_serial_paths()
    except Exception:
        return 0, 0, False
    held = _serial_paths_held_open(candidates)
    if held is None:
        return 0, 0, False
    free = sorted(candidates - held)
    if not free:
        return 0, 0, False

    per_port_timeout = max(timeout / len(free), 0.15)

    for path in free:
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

    The mavlink service publishes a ~10 Hz state snapshot to the unix
    socket at `/run/ados/state.sock`. The first frame on connect is the
    most recent snapshot, so a single short read is enough to tell us
    whether the FC is connected. The frame decode (the v1 JSON / v2
    length-prefixed msgpack sniff) is shared with the async state reader via
    ``ados.core.ipc._read_state_frame_from_socket`` so there is a single
    Python decoder for the state wire.

    On a brand-new boot the socket may not exist yet; that's expected and
    the probe returns zero so the decision falls back to the persistence
    tiebreaker.
    """
    state_sock = "/run/ados/state.sock"
    if not Path(state_sock).exists():
        return 0, 0, False

    try:
        from ados.core.ipc import _read_state_frame_from_socket
    except Exception:  # pragma: no cover - ipc deps unavailable in bare dry-run
        return 0, 0, False

    deadline = time.monotonic() + timeout
    data: Any = None
    try:
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            sock.settimeout(timeout)
            sock.connect(state_sock)
            data = _read_state_frame_from_socket(sock, deadline)
        finally:
            try:
                sock.close()
            except OSError:
                pass
    except (OSError, TimeoutError):
        return 0, 0, False

    if isinstance(data, dict) and bool(data.get("fc_connected")):
        return 0, 3, True
    return 0, 0, False
