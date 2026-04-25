"""USB composite gadget lifecycle (spec 07-usb-tether-gadget.md).

Builds a libcomposite CDC-NCM + RNDIS gadget on the Pi 4B USB-C OTG
port. The host (Mac, Windows, Linux, Android 11+) picks whichever
function it supports. Modern macOS and Win11 pick CDC-NCM; older
Win10 falls back to RNDIS.

Gadget is created under /sys/kernel/config/usb_gadget/ados_gs and
bound to the first UDC listed in /sys/class/udc. After bind, usb0
gets 192.168.7.1/24 and a single-host dnsmasq serves 192.168.7.2 to
the tethered host over DHCP.

Requires dwc2 overlay (dtoverlay=dwc2 in /boot/firmware/config.txt
plus modules-load=dwc2 in cmdline.txt). If configfs is not writable
the process exits non-zero so systemd can surface the error and the
setup webapp can show "USB tether unavailable, check boot config".

Runs as root. If started as non-root, logs a warning and still
attempts the operation; configfs writes and dnsmasq spawn will then
fail cleanly with EACCES.

Android 11+ tether behavior
---------------------------
Android 11 and newer phones expose CDC-NCM as "USB Ethernet". When
the user plugs a USB-C cable between the phone and the ground
station, Android prompts "Allow USB Ethernet" on the notification
shade. Tap allow, then open the setup webapp at
http://192.168.7.1/. The phone receives 192.168.7.2 from the
dnsmasq served here. Android 10 and older do not support this path
reliably and should use the WiFi AP instead. The setup webapp
surfaces this as a hint on the landing page.
"""

from __future__ import annotations

import asyncio
import os
import shutil
import signal
import subprocess
import sys
from pathlib import Path

import structlog

from ados.core.config import load_config
from ados.core.logging import configure_logging, get_logger
from ados.core.paths import DNSMASQ_USB0_CONF, DNSMASQ_USB0_PID

log = get_logger("ground_station.usb_gadget")

GADGET_ROOT = Path("/sys/kernel/config/usb_gadget")
GADGET_NAME = "ados_gs"
GADGET_DIR = GADGET_ROOT / GADGET_NAME

USB_INTERFACE = "usb0"
USB_IP = "192.168.7.1"
USB_NETMASK_PREFIX = 24
USB_SUBNET = "192.168.7.0/24"
DHCP_RANGE_START = "192.168.7.2"
DHCP_RANGE_END = "192.168.7.2"

DNSMASQ_CONF_PATH = DNSMASQ_USB0_CONF
DNSMASQ_PID_PATH = DNSMASQ_USB0_PID

# USB descriptor values, straight from 07-usb-tether-gadget.md
ID_VENDOR = "0x1d6b"      # Linux Foundation
ID_PRODUCT = "0x0104"     # Multifunction composite gadget
BCD_DEVICE = "0x0100"
BCD_USB = "0x0200"
STR_MANUFACTURER = "ADOS Ground Station"
STR_PRODUCT = "ADOS GS"
CONFIG_MAX_POWER = "250"  # mA


def _write(path: Path, value: str) -> None:
    """Write a string to a configfs attribute, creating no trailing newline."""
    path.write_text(value)


def _warn_if_not_root() -> None:
    if os.geteuid() != 0:
        log.warning(
            "usb_gadget_not_root",
            euid=os.geteuid(),
            msg="configfs and dnsmasq require root; attempt will fail",
        )


def configfs_available() -> bool:
    """Return True if /sys/kernel/config/usb_gadget is present and writable."""
    if not GADGET_ROOT.exists():
        return False
    # A stat + write test would require touching the filesystem. Existence
    # of the directory is a proxy; dwc2 + configfs-usb-gadget modules must
    # both be loaded for it to appear.
    return GADGET_ROOT.is_dir()


def _pick_udc() -> str | None:
    udc_dir = Path("/sys/class/udc")
    if not udc_dir.exists():
        return None
    entries = sorted(p.name for p in udc_dir.iterdir())
    return entries[0] if entries else None


class UsbGadgetManager:
    """libcomposite composite gadget setup for the ground station profile."""

    def __init__(self) -> None:
        self._dnsmasq: subprocess.Popen | None = None
        self._bound = False

    def setup(self) -> bool:
        """Build the gadget tree and bind it to a UDC.

        Returns True on success. On failure returns False and leaves
        partial state behind; teardown() is safe to call regardless.
        """
        _warn_if_not_root()

        if not configfs_available():
            log.error(
                "usb_gadget_configfs_missing",
                path=str(GADGET_ROOT),
                hint="dtoverlay=dwc2 in /boot/firmware/config.txt + reboot",
            )
            return False

        try:
            # Idempotent: if a previous run left the gadget in place,
            # teardown first so we rebuild from scratch.
            if GADGET_DIR.exists():
                log.info("usb_gadget_stale_found", path=str(GADGET_DIR))
                self.teardown()

            GADGET_DIR.mkdir(parents=True, exist_ok=False)

            _write(GADGET_DIR / "idVendor", ID_VENDOR)
            _write(GADGET_DIR / "idProduct", ID_PRODUCT)
            _write(GADGET_DIR / "bcdDevice", BCD_DEVICE)
            _write(GADGET_DIR / "bcdUSB", BCD_USB)

            strings_dir = GADGET_DIR / "strings" / "0x409"
            strings_dir.mkdir(parents=True, exist_ok=True)
            _write(strings_dir / "manufacturer", STR_MANUFACTURER)
            _write(strings_dir / "product", STR_PRODUCT)

            # NCM function (macOS, Win11, Linux, Android 11+)
            (GADGET_DIR / "functions" / "ncm.usb0").mkdir(
                parents=True, exist_ok=True
            )
            # RNDIS function (Win10 fallback)
            (GADGET_DIR / "functions" / "rndis.usb0").mkdir(
                parents=True, exist_ok=True
            )

            config_dir = GADGET_DIR / "configs" / "c.1"
            config_dir.mkdir(parents=True, exist_ok=True)
            _write(config_dir / "MaxPower", CONFIG_MAX_POWER)

            # Link both functions into the one composite config
            for fname in ("ncm.usb0", "rndis.usb0"):
                link = config_dir / fname
                if not link.exists():
                    os.symlink(
                        GADGET_DIR / "functions" / fname,
                        link,
                    )

            udc = _pick_udc()
            if udc is None:
                log.error(
                    "usb_gadget_no_udc",
                    hint="no /sys/class/udc entries; dwc2 driver not bound",
                )
                return False

            _write(GADGET_DIR / "UDC", udc)
            self._bound = True
            log.info("usb_gadget_bound", udc=udc)

        except OSError as exc:
            log.error("usb_gadget_setup_failed", error=str(exc))
            return False

        # Bring up the usb0 link and assign the static IP.
        if not self._bring_up_interface():
            return False

        # Start a single-host dnsmasq. DHCP is the cleanest way to get
        # macOS, Windows, Linux, and Android 11+ to configure 192.168.7.2
        # without any host-side setup.
        if not self._start_dnsmasq():
            log.warning(
                "usb_gadget_dnsmasq_failed",
                msg="gadget is up but host may not auto-configure IP",
            )

        return True

    def _bring_up_interface(self) -> bool:
        """Assign 192.168.7.1/24 to usb0 and bring the link up."""
        ip_bin = shutil.which("ip") or "/sbin/ip"
        try:
            # The usb0 interface only appears after UDC bind. Give the
            # kernel a moment to enumerate the netdev.
            for _ in range(20):
                if Path(f"/sys/class/net/{USB_INTERFACE}").exists():
                    break
                # Busy-wait without sleep in the setup path; the caller
                # already ran through configfs writes synchronously.
                import time as _time
                _time.sleep(0.1)
            else:
                log.error(
                    "usb_gadget_interface_missing",
                    interface=USB_INTERFACE,
                )
                return False

            subprocess.run(
                [ip_bin, "addr", "flush", "dev", USB_INTERFACE],
                check=False,
            )
            subprocess.run(
                [
                    ip_bin,
                    "addr",
                    "add",
                    f"{USB_IP}/{USB_NETMASK_PREFIX}",
                    "dev",
                    USB_INTERFACE,
                ],
                check=True,
            )
            subprocess.run(
                [ip_bin, "link", "set", USB_INTERFACE, "up"],
                check=True,
            )
            log.info(
                "usb_gadget_interface_up",
                interface=USB_INTERFACE,
                ip=USB_IP,
            )
            return True
        except subprocess.CalledProcessError as exc:
            log.error("usb_gadget_ip_failed", error=str(exc))
            return False

    def _start_dnsmasq(self) -> bool:
        """Spawn dnsmasq on usb0 with a single-host DHCP range."""
        binary = shutil.which("dnsmasq")
        if not binary:
            log.error(
                "dnsmasq_not_found",
                hint="apt install dnsmasq-base or dnsmasq",
            )
            return False

        DNSMASQ_CONF_PATH.parent.mkdir(parents=True, exist_ok=True)
        conf = "\n".join([
            f"interface={USB_INTERFACE}",
            "bind-interfaces",
            "except-interface=lo",
            f"listen-address={USB_IP}",
            f"dhcp-range={DHCP_RANGE_START},{DHCP_RANGE_END},255.255.255.0,12h",
            f"dhcp-option=option:router,{USB_IP}",
            f"dhcp-option=option:dns-server,{USB_IP}",
            "no-resolv",
            "no-hosts",
            "log-dhcp",
            f"pid-file={DNSMASQ_PID_PATH}",
            "",
        ])
        DNSMASQ_CONF_PATH.write_text(conf)

        try:
            self._dnsmasq = subprocess.Popen(
                [
                    binary,
                    "--keep-in-foreground",
                    "--conf-file=" + str(DNSMASQ_CONF_PATH),
                ],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
            )
            log.info(
                "usb_gadget_dnsmasq_started",
                pid=self._dnsmasq.pid,
                conf=str(DNSMASQ_CONF_PATH),
            )
            return True
        except OSError as exc:
            log.error("dnsmasq_spawn_failed", error=str(exc))
            return False

    def teardown(self) -> None:
        """Unbind the gadget, tear down dnsmasq, delete the configfs tree."""
        # Stop dnsmasq first so the DHCP lease does not linger when the
        # host sees the interface disappear.
        if self._dnsmasq is not None and self._dnsmasq.poll() is None:
            try:
                self._dnsmasq.terminate()
                self._dnsmasq.wait(timeout=5.0)
                log.info("usb_gadget_dnsmasq_stopped")
            except subprocess.TimeoutExpired:
                try:
                    self._dnsmasq.kill()
                except ProcessLookupError:
                    pass
                try:
                    self._dnsmasq.wait(timeout=1.0)
                except subprocess.TimeoutExpired:
                    pass
                log.warning("usb_gadget_dnsmasq_killed")
            except ProcessLookupError:
                pass
        self._dnsmasq = None

        # Unbind from UDC
        udc_file = GADGET_DIR / "UDC"
        if udc_file.exists():
            try:
                udc_file.write_text("\n")
            except OSError as exc:
                log.debug("usb_gadget_unbind_failed", error=str(exc))

        # Remove symlinks in configs/c.1 before rmdir
        config_dir = GADGET_DIR / "configs" / "c.1"
        if config_dir.exists():
            for link in config_dir.iterdir():
                if link.is_symlink():
                    try:
                        link.unlink()
                    except OSError:
                        pass
            try:
                config_dir.rmdir()
            except OSError as exc:
                log.debug("usb_gadget_config_rmdir_failed", error=str(exc))

        # Remove function directories
        functions_dir = GADGET_DIR / "functions"
        if functions_dir.exists():
            for fn in functions_dir.iterdir():
                try:
                    fn.rmdir()
                except OSError:
                    pass

        # Remove strings
        strings_dir = GADGET_DIR / "strings" / "0x409"
        if strings_dir.exists():
            try:
                strings_dir.rmdir()
            except OSError:
                pass

        # Remove the top-level gadget dir last
        if GADGET_DIR.exists():
            try:
                GADGET_DIR.rmdir()
                log.info("usb_gadget_removed")
            except OSError as exc:
                log.debug("usb_gadget_rmdir_failed", error=str(exc))

        self._bound = False


async def main() -> None:
    """Service entry point. Sets up the gadget, then sleeps for signals.

    systemd keeps this process alive so teardown runs on stop. The work
    is synchronous, but we still run under asyncio for consistency with
    the other ground-station services and to use asyncio signal handling.
    """
    config = load_config()
    configure_logging(config.logging.level)
    slog = structlog.get_logger()
    slog.info("usb_gadget_service_starting")

    shutdown = asyncio.Event()
    loop = asyncio.get_event_loop()
    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, shutdown.set)

    manager = UsbGadgetManager()
    ok = manager.setup()
    if not ok:
        slog.error("usb_gadget_setup_failed")
        sys.exit(2)

    slog.info("usb_gadget_service_ready", ip=USB_IP, interface=USB_INTERFACE)

    await shutdown.wait()

    slog.info("usb_gadget_service_stopping")
    manager.teardown()
    slog.info("usb_gadget_service_stopped")


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
    sys.exit(0)
