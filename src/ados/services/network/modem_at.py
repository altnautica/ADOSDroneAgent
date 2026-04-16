"""4G LTE modem service — AT commands, APN auto-detection, signal monitoring.

AT-command-based modem manager for direct serial communication with
Quectel and similar USB modems.

Note: The full agent also has ``ados.hal.modem`` which uses mmcli
(ModemManager) for modem detection. This module provides an alternative
AT-command-based approach that works without ModemManager installed,
useful on minimal embedded Linux images.
"""

from __future__ import annotations

import asyncio
import glob
import sys
import time
from pathlib import Path

from ados.core.logging import get_logger

log = get_logger("services.network.modem_at")

# Known Quectel USB VID:PID patterns
QUECTEL_PATTERNS = ["2C7C"]

# ICCID prefix -> APN mapping (Indian and Chinese carriers)
APN_MAP = {
    "8991": "jionet",             # Jio (India)
    "8991010": "airtelgprs.com",  # Airtel (India, more specific prefix)
    "8991100": "internet",        # BSNL/Vi (India)
    "8986": "cmnet",              # China Mobile
    "8985": "3gnet",              # China Unicom
    "8984": "ctnet",              # China Telecom
}


class AtModemService:
    """Manages 4G/LTE USB modem via AT commands.

    Parameters
    ----------
    enabled : str
        "auto" to auto-detect, "true" to force enable, "false" to disable.
    apn : str
        APN string, or "auto" to detect from SIM ICCID.
    signal_poll_interval : float
        Seconds between AT+CSQ signal polls.
    shutdown_event : asyncio.Event or None
        Event to signal graceful shutdown.
    """

    def __init__(
        self,
        enabled: str = "auto",
        apn: str = "auto",
        signal_poll_interval: float = 5.0,
        shutdown_event: asyncio.Event | None = None,
    ):
        self.enabled = enabled
        self.apn = apn
        self.signal_poll_interval = signal_poll_interval
        self._shutdown = shutdown_event or asyncio.Event()
        self._serial: object | None = None

        # Public state (read by status API)
        self.modem_present: bool = False
        self.modem_connected: bool = False
        self.modem_ip: str = ""
        self.signal_dbm: int = 0

    async def run(self) -> None:
        if sys.platform != "linux":
            log.info("modem_skipped", reason="not Linux")
            await self._shutdown.wait()
            return

        if self.enabled == "false":
            log.info("modem_disabled")
            await self._shutdown.wait()
            return

        # 1. Find modem serial port
        modem_port = await self._find_modem()
        if not modem_port:
            log.info("no_modem_detected")
            self.modem_present = False
            await self._shutdown.wait()
            return

        self.modem_present = True
        log.info("modem_found", port=modem_port)

        # 2. Open serial and initialize
        import serial

        try:
            self._serial = await asyncio.to_thread(
                serial.Serial, modem_port, 115200, timeout=1
            )
        except Exception as e:
            log.error("modem_open_failed", port=modem_port, error=str(e))
            await self._shutdown.wait()
            return

        try:
            # 3. Basic init
            await self._at("ATE0")      # Echo off
            await self._at("AT+CFUN=1")  # Full functionality

            # 4. Check SIM
            resp = await self._at("AT+CPIN?")
            if "READY" not in resp:
                log.error("sim_not_ready", response=resp)
                await self._shutdown.wait()
                return

            # 5. Set APN
            apn = self.apn
            if apn == "auto":
                apn = await self._detect_apn()
            await self._at(f'AT+CGDCONT=1,"IP","{apn}"')
            log.info("apn_set", apn=apn)

            # 6. Enable data
            await self._at("AT+CGACT=1,1")

            # 7. Wait for usb0 interface
            if await self._wait_for_interface("usb0", timeout=30):
                self.modem_connected = True
                # Get IP
                ip = await self._get_interface_ip("usb0")
                self.modem_ip = ip or ""
                log.info("modem_connected", ip=ip, apn=apn)

                # 8. Set routing (usb0 as default, wlan0 as local)
                await self._setup_routing()
            else:
                log.warning("modem_no_interface", hint="usb0 did not appear after AT+CGACT")

            # 9. Signal monitoring loop
            while not self._shutdown.is_set():
                await self._poll_signal()
                try:
                    await asyncio.wait_for(
                        self._shutdown.wait(),
                        timeout=self.signal_poll_interval,
                    )
                    break  # Shutdown signaled
                except asyncio.TimeoutError:
                    pass  # Continue polling
        finally:
            if self._serial and self._serial.is_open:
                self._serial.close()
            self.modem_connected = False

    async def _find_modem(self) -> str | None:
        """Find Quectel modem AT command port."""
        candidates = sorted(glob.glob("/dev/ttyUSB*") + glob.glob("/dev/ttyACM*"))
        if not candidates:
            return None

        # Try to identify the AT command port (usually ttyUSB2 for Quectel EC200)
        for port in candidates:
            try:
                import serial

                s = await asyncio.to_thread(serial.Serial, port, 115200, timeout=0.5)
                await asyncio.to_thread(s.write, b"AT\r\n")
                await asyncio.sleep(0.3)
                resp = await asyncio.to_thread(s.read, 64)
                s.close()
                if b"OK" in resp:
                    return port
            except Exception:
                continue

        # Fallback: return last ttyUSB port (often the AT port)
        usb_ports = [p for p in candidates if "ttyUSB" in p]
        return usb_ports[-1] if usb_ports else None

    async def _at(self, cmd: str, timeout: float = 5.0) -> str:
        """Send AT command and return response."""
        if not self._serial or not self._serial.is_open:
            return ""

        # Drain input buffer
        await asyncio.to_thread(self._serial.reset_input_buffer)

        # Send command
        await asyncio.to_thread(self._serial.write, f"{cmd}\r\n".encode())

        # Read response
        deadline = time.monotonic() + timeout
        response = ""
        while time.monotonic() < deadline:
            if await asyncio.to_thread(lambda: self._serial.in_waiting > 0):
                data = await asyncio.to_thread(
                    self._serial.read, self._serial.in_waiting
                )
                response += data.decode(errors="replace")
                if "OK" in response or "ERROR" in response:
                    break
            await asyncio.sleep(0.1)

        return response.strip()

    async def _detect_apn(self) -> str:
        """Auto-detect APN from SIM ICCID."""
        resp = await self._at("AT+QCCID")
        for line in resp.splitlines():
            if "CCID" in line.upper():
                iccid = "".join(c for c in line.split(":")[-1].strip() if c.isdigit())
                if len(iccid) < 15:
                    log.warning("iccid_too_short", iccid=iccid, length=len(iccid))
                    continue
                # Match longest prefix first
                for prefix in sorted(APN_MAP.keys(), key=len, reverse=True):
                    if iccid.startswith(prefix):
                        detected = APN_MAP[prefix]
                        log.info("apn_auto_detected", iccid_prefix=prefix, apn=detected)
                        return detected
                log.warning("iccid_no_carrier_match", iccid_prefix=iccid[:8])
        log.warning("apn_auto_detect_failed", fallback="internet")
        return "internet"

    async def _wait_for_interface(self, iface: str, timeout: int = 30) -> bool:
        """Wait for a network interface to appear."""
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if Path(f"/sys/class/net/{iface}").exists():
                return True
            await asyncio.sleep(1)
        return False

    async def _get_interface_ip(self, iface: str) -> str | None:
        """Get IPv4 address of an interface."""
        try:
            proc = await asyncio.create_subprocess_exec(
                "ip", "-4", "addr", "show", iface,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.DEVNULL,
            )
            stdout, _ = await proc.communicate()
            for line in stdout.decode().splitlines():
                line = line.strip()
                if line.startswith("inet "):
                    return line.split()[1].split("/")[0]
        except Exception:
            pass
        return None

    async def _setup_routing(self) -> None:
        """Set usb0 as default route, wlan0 as local only."""
        cmds = [
            ["ip", "route", "del", "default"],
            ["ip", "route", "add", "default", "dev", "usb0", "metric", "100"],
        ]
        for cmd in cmds:
            proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            _, stderr = await proc.communicate()
            if proc.returncode != 0 and "del" not in cmd:
                log.warning(
                    "routing_cmd_failed",
                    cmd=" ".join(cmd),
                    rc=proc.returncode,
                    stderr=stderr.decode(errors="replace").strip(),
                )

    async def _poll_signal(self) -> None:
        """Read signal strength from modem."""
        resp = await self._at("AT+CSQ", timeout=3)
        for line in resp.splitlines():
            if "+CSQ:" in line:
                try:
                    val = int(line.split(":")[1].strip().split(",")[0])
                    self.signal_dbm = -113 + (val * 2)  # Convert to dBm
                except (ValueError, IndexError):
                    pass
