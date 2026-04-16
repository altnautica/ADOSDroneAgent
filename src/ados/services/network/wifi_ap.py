"""WiFi Access Point manager — hostapd + dnsmasq lifecycle.

Manages a WiFi AP for direct GCS connection when no existing network
is available (field operations).
"""

from __future__ import annotations

import asyncio
import sys
from pathlib import Path

from ados.core.logging import get_logger

log = get_logger("services.network.wifi_ap")

HOSTAPD_CONF = "/tmp/ados-hostapd.conf"
DNSMASQ_CONF = "/tmp/ados-dnsmasq.conf"


class WifiApManager:
    """Manages WiFi AP via hostapd + dnsmasq."""

    def __init__(
        self,
        ssid_prefix: str = "ADOS",
        password: str = "",
        channel: int = 6,
        ap_ip: str = "192.168.4.1",
        captive_portal: bool = True,
        device_id: str = "",
        shutdown_event: asyncio.Event | None = None,
    ):
        self.ssid_prefix = ssid_prefix
        self.password = password
        self.channel = channel
        self.ap_ip = ap_ip
        self.captive_portal = captive_portal
        self.device_id = device_id
        self._shutdown = shutdown_event or asyncio.Event()
        self._hostapd_proc: asyncio.subprocess.Process | None = None
        self._dnsmasq_proc: asyncio.subprocess.Process | None = None

    async def run(self) -> None:
        if sys.platform != "linux":
            log.info("wifi_ap_skipped", reason="not Linux")
            await self._shutdown.wait()
            return

        # 1. Find wireless interface
        iface = await self._find_wireless_iface()
        if not iface:
            log.warning("no_wireless_interface")
            await self._shutdown.wait()
            return

        # 2. Configure interface IP
        await self._configure_interface(iface)

        # 3. Generate configs
        suffix = self.device_id[-4:] if len(self.device_id) >= 4 else self.device_id
        ssid = f"{self.ssid_prefix}-{suffix}"
        self._write_hostapd_conf(iface, ssid)
        self._write_dnsmasq_conf(iface)

        # 4. Start daemons
        await self._start_hostapd()
        await self._start_dnsmasq()
        log.info("wifi_ap_started", ssid=ssid, ip=self.ap_ip, iface=iface)

        try:
            await self._shutdown.wait()
        finally:
            await self._stop()

    async def _find_wireless_iface(self) -> str | None:
        """Find first wireless interface (wlan0, wlp*, etc)."""
        try:
            proc = await asyncio.create_subprocess_exec(
                "iw", "dev",
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            stdout, _ = await asyncio.wait_for(proc.communicate(), timeout=5)
            for line in stdout.decode().splitlines():
                line = line.strip()
                if line.startswith("Interface"):
                    return line.split()[-1]
        except (FileNotFoundError, asyncio.TimeoutError):
            pass

        # Fallback: check sysfs
        for candidate in ("wlan0", "wlan1"):
            if Path(f"/sys/class/net/{candidate}").exists():
                return candidate
        return None

    async def _configure_interface(self, iface: str) -> None:
        """Set IP on wireless interface."""
        cmds = [
            ["ip", "addr", "flush", "dev", iface],
            ["ip", "addr", "add", f"{self.ap_ip}/24", "dev", iface],
            ["ip", "link", "set", iface, "up"],
        ]
        for cmd in cmds:
            proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.DEVNULL,
            )
            await proc.wait()

    def _write_hostapd_conf(self, iface: str, ssid: str) -> None:
        conf = (
            f"interface={iface}\n"
            f"driver=nl80211\n"
            f"ssid={ssid}\n"
            f"hw_mode=g\n"
            f"channel={self.channel}\n"
            f"wmm_enabled=0\n"
            f"auth_algs=1\n"
        )
        if self.password:
            conf += (
                f"wpa=2\n"
                f"wpa_passphrase={self.password}\n"
                f"wpa_key_mgmt=WPA-PSK\n"
                f"rsn_pairwise=CCMP\n"
            )
        Path(HOSTAPD_CONF).write_text(conf)

    def _write_dnsmasq_conf(self, iface: str) -> None:
        base_ip = ".".join(self.ap_ip.split(".")[:3])
        conf = (
            f"interface={iface}\n"
            f"dhcp-range={base_ip}.10,{base_ip}.50,24h\n"
        )
        if self.captive_portal:
            conf += f"address=/#/{self.ap_ip}\n"
        Path(DNSMASQ_CONF).write_text(conf)

    async def _start_hostapd(self) -> None:
        self._hostapd_proc = await asyncio.create_subprocess_exec(
            "hostapd", HOSTAPD_CONF,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )

    async def _start_dnsmasq(self) -> None:
        self._dnsmasq_proc = await asyncio.create_subprocess_exec(
            "dnsmasq", "-C", DNSMASQ_CONF, "--no-daemon", "--log-queries",
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )

    async def _stop(self) -> None:
        for name, proc in [("hostapd", self._hostapd_proc), ("dnsmasq", self._dnsmasq_proc)]:
            if proc and proc.returncode is None:
                proc.terminate()
                try:
                    await asyncio.wait_for(proc.wait(), timeout=5)
                except asyncio.TimeoutError:
                    proc.kill()
                log.info("daemon_stopped", daemon=name)
        # Cleanup temp files
        for f in (HOSTAPD_CONF, DNSMASQ_CONF):
            Path(f).unlink(missing_ok=True)
