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

# The sysfs network root. Named so a test can point interface resolution at a
# fixture tree instead of the host's real /sys.
_NET_ROOT = Path("/sys/class/net")


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
        interface: str = "",
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
        # Tracked by `_configure_interface` so `_stop` can release the
        # AP-side IP when the captive portal exits. Without this the
        # `192.168.4.1/24` address lingers on wlan0 after stop, which
        # avahi continues to publish via mDNS and confuses LAN clients.
        self._iface: str | None = None
        # The operator's `network.hotspot.interface` pin, empty when unset. The
        # hostapd manager passes the same value to the same resolver; dropping
        # it here would make the two disagree the moment a pin is set, which is
        # exactly the drift the shared resolver exists to prevent.
        self.interface = interface

    async def run(self) -> None:
        if sys.platform != "linux":
            log.info("wifi_ap_skipped", reason="not Linux")
            await self._shutdown.wait()
            return

        # 1. Resolve the wireless interface by DRIVER
        iface = self._find_wireless_iface()
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

    def _find_wireless_iface(self) -> str | None:
        """The interface the AP should bind, resolved by driver.

        Was "the first `iw dev` Interface line, else literal wlan0/wlan1", which
        is readdir order: on a ground station carrying both an onboard chip and a
        USB long-range radio that picked the aircraft's flight link roughly one
        boot in three, and running hostapd on it takes the aircraft down.

        Delegates to the single AP-interface resolver, passing the operator's
        interface pin so this cannot drift from the hostapd manager's choice.
        Returns None only when the resolver's fallback names an interface this
        box does not have, which is the same "no wireless" outcome the caller
        already handles.
        """
        from ados.services.ground_station.hostapd_manager import (
            resolve_ap_interface,
        )

        iface = resolve_ap_interface(self.interface, net_root=_NET_ROOT)
        if not (_NET_ROOT / iface).exists():
            return None
        return iface

    async def _configure_interface(self, iface: str) -> None:
        """Set IP on wireless interface."""
        self._iface = iface
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
                except TimeoutError:
                    proc.kill()
                log.info("daemon_stopped", daemon=name)
        # Release the AP IP so it does not linger on the interface
        # after the captive portal exits. Idempotent — `ip addr del`
        # exits non-zero if the address is already absent and we
        # ignore the return code.
        if self._iface:
            try:
                proc = await asyncio.create_subprocess_exec(
                    "ip", "addr", "del", f"{self.ap_ip}/24", "dev", self._iface,
                    stdout=asyncio.subprocess.DEVNULL,
                    stderr=asyncio.subprocess.DEVNULL,
                )
                await asyncio.wait_for(proc.wait(), timeout=2)
            except (TimeoutError, FileNotFoundError):
                pass
            log.info("ap_ip_released", ip=self.ap_ip, iface=self._iface)
        # Cleanup temp files
        for f in (HOSTAPD_CONF, DNSMASQ_CONF):
            Path(f).unlink(missing_ok=True)
