"""WiFi AP lifecycle for the ground-station profile.

The ground-station Pi 4B runs `hostapd` on the onboard wlan0 so phones,
tablets, and laptops can join a stable SSID (`ADOS-GS-<short_id>`) and
reach the setup webapp, WHEP video, and agent REST API. The RTL8812
USB adapter is reserved for monitor-mode WFB-ng RX by `wfb_rx.py` and
is never touched here.

Lifecycle:
1. Load or generate a per-device passphrase at `/etc/ados/ap-passphrase`.
2. Render `hostapd.conf` at `/etc/ados/hostapd-gs.conf` (SSID, channel,
   WPA2-PSK, country IN).
3. Render a matching `dnsmasq` conf at `/etc/ados/dnsmasq-gs.conf` with
   DHCP range 192.168.4.10-100, lease 12h.
4. Assign 192.168.4.1/24 to wlan0.
5. Start hostapd and dnsmasq via systemd units
   (`data/systemd/ados-hostapd.service`).
6. Scrape `iw dev wlan0 station dump` for connected client MACs.

Exits non-zero if config write fails. systemd restart policy handles
the retry loop, same pattern as the WFB RX service.
"""

from __future__ import annotations

import asyncio
import os
import re
import secrets
import signal
import subprocess
import sys
from pathlib import Path

import structlog

from ados.core.config import load_config
from ados.core.logging import configure_logging, get_logger
from ados.core.paths import (
    AP_PASSPHRASE_PATH,
    DNSMASQ_CONF_PATH,
    HOSTAPD_CONF_PATH,
)

log = get_logger("ground_station.hostapd")

# Passphrase alphabet with ambiguous glyphs stripped (no 0/O, 1/l/I).
_PASSPHRASE_ALPHABET = (
    "ABCDEFGHJKLMNPQRSTUVWXYZ"
    "abcdefghijkmnopqrstuvwxyz"
    "23456789"
)
_PASSPHRASE_LEN = 10

_PASSPHRASE_PATH = AP_PASSPHRASE_PATH
_HOSTAPD_CONF_PATH = HOSTAPD_CONF_PATH
_DNSMASQ_CONF_PATH = DNSMASQ_CONF_PATH

_AP_IFACE = "wlan0"
_AP_ADDR = "192.168.4.1"
_AP_CIDR = f"{_AP_ADDR}/24"
_DHCP_RANGE = "192.168.4.10,192.168.4.100,12h"

_HOSTAPD_UNIT = "ados-hostapd.service"
_DNSMASQ_UNIT = "ados-dnsmasq-gs.service"


def _short_id(device_id: str) -> str:
    """Return the first 4 hex chars of device_id, uppercased.

    Falls back to a zero-padded placeholder if device_id is empty or
    has fewer than 4 hex chars after stripping non-hex characters.
    """
    hex_only = re.sub(r"[^0-9a-fA-F]", "", device_id or "")
    if len(hex_only) >= 4:
        return hex_only[:4].upper()
    return (hex_only + "0000")[:4].upper()


def _build_ssid(device_id: str) -> str:
    return f"ADOS-GS-{_short_id(device_id)}"


class HostapdManager:
    """Manages hostapd + dnsmasq for the ground-station AP.

    One instance per agent. Idempotent: `apply_ap_config` and
    `write_config` both tolerate being called repeatedly.
    """

    def __init__(
        self,
        device_id: str,
        ssid: str | None = None,
        channel: int = 6,
        interface: str = _AP_IFACE,
    ) -> None:
        self._device_id = device_id
        self._ssid = ssid or _build_ssid(device_id)
        self._channel = channel
        self._interface = interface
        self._passphrase: str = ""
        self._running = False

    @property
    def ssid(self) -> str:
        return self._ssid

    @property
    def channel(self) -> int:
        return self._channel

    @property
    def interface(self) -> str:
        return self._interface

    @property
    def passphrase(self) -> str:
        return self._passphrase

    def ensure_passphrase(self) -> str:
        """Load the persisted passphrase or generate one on first boot.

        The passphrase is never rotated automatically. Once written it
        is the stable credential for the life of the device. File mode
        0600 owned by the agent user (root under systemd).
        """
        if _PASSPHRASE_PATH.exists():
            try:
                existing = _PASSPHRASE_PATH.read_text(encoding="utf-8").strip()
                if existing:
                    self._passphrase = existing
                    log.info("ap_passphrase_loaded", path=str(_PASSPHRASE_PATH))
                    return self._passphrase
            except OSError as exc:
                log.warning(
                    "ap_passphrase_read_failed",
                    path=str(_PASSPHRASE_PATH),
                    error=str(exc),
                )

        new_pass = "".join(
            secrets.choice(_PASSPHRASE_ALPHABET) for _ in range(_PASSPHRASE_LEN)
        )
        try:
            _PASSPHRASE_PATH.parent.mkdir(parents=True, exist_ok=True)
            _PASSPHRASE_PATH.write_text(new_pass + "\n", encoding="utf-8")
            os.chmod(_PASSPHRASE_PATH, 0o600)
            log.info("ap_passphrase_generated", path=str(_PASSPHRASE_PATH))
        except OSError as exc:
            log.error(
                "ap_passphrase_write_failed",
                path=str(_PASSPHRASE_PATH),
                error=str(exc),
            )
        self._passphrase = new_pass
        return self._passphrase

    def _render_hostapd_conf(self) -> str:
        """Return the hostapd.conf body as a string."""
        lines = [
            f"# ADOS Ground Station hostapd config for {self._ssid}",
            f"interface={self._interface}",
            "driver=nl80211",
            f"ssid={self._ssid}",
            "hw_mode=g",
            f"channel={self._channel}",
            "country_code=IN",
            "ieee80211n=1",
            "ieee80211d=1",
            "wmm_enabled=1",
            "auth_algs=1",
            "macaddr_acl=0",
            "ignore_broadcast_ssid=0",
            "wpa=2",
            f"wpa_passphrase={self._passphrase}",
            "wpa_key_mgmt=WPA-PSK",
            "wpa_pairwise=CCMP",
            "rsn_pairwise=CCMP",
            # Stable BSSID. MAC randomization stays off so clients that
            # remember the network reconnect cleanly across reboots.
            "",
        ]
        return "\n".join(lines)

    def _render_dnsmasq_conf(self) -> str:
        """Return the dnsmasq conf body as a string."""
        lines = [
            f"# ADOS Ground Station DHCP for {self._interface}",
            f"interface={self._interface}",
            "bind-interfaces",
            "except-interface=lo",
            f"dhcp-range={_DHCP_RANGE}",
            f"dhcp-option=3,{_AP_ADDR}",
            f"dhcp-option=6,{_AP_ADDR}",
            "domain-needed",
            "bogus-priv",
            "no-resolv",
            "",
        ]
        return "\n".join(lines)

    def write_config(self) -> Path:
        """Render and write hostapd + dnsmasq conf files.

        Returns the hostapd conf path. Creates the /etc/ados directory
        if missing. Passphrase is ensured before the first render.
        """
        if not self._passphrase:
            self.ensure_passphrase()

        _HOSTAPD_CONF_PATH.parent.mkdir(parents=True, exist_ok=True)

        hostapd_body = self._render_hostapd_conf()
        dnsmasq_body = self._render_dnsmasq_conf()

        try:
            _HOSTAPD_CONF_PATH.write_text(hostapd_body, encoding="utf-8")
            os.chmod(_HOSTAPD_CONF_PATH, 0o600)
        except OSError as exc:
            log.error(
                "hostapd_conf_write_failed",
                path=str(_HOSTAPD_CONF_PATH),
                error=str(exc),
            )
            raise

        try:
            _DNSMASQ_CONF_PATH.write_text(dnsmasq_body, encoding="utf-8")
            os.chmod(_DNSMASQ_CONF_PATH, 0o644)
        except OSError as exc:
            log.error(
                "dnsmasq_conf_write_failed",
                path=str(_DNSMASQ_CONF_PATH),
                error=str(exc),
            )
            raise

        log.info(
            "ap_config_written",
            hostapd=str(_HOSTAPD_CONF_PATH),
            dnsmasq=str(_DNSMASQ_CONF_PATH),
            ssid=self._ssid,
            channel=self._channel,
        )
        return _HOSTAPD_CONF_PATH

    def _assign_ip(self) -> bool:
        """Assign the AP gateway address to wlan0.

        Idempotent: if the address is already present the command is a
        no-op that returns non-zero, which we swallow.
        """
        try:
            subprocess.run(
                ["ip", "addr", "add", _AP_CIDR, "dev", self._interface],
                check=False,
                capture_output=True,
                timeout=5,
            )
            subprocess.run(
                ["ip", "link", "set", self._interface, "up"],
                check=False,
                capture_output=True,
                timeout=5,
            )
            return True
        except (OSError, subprocess.SubprocessError) as exc:
            log.warning("ap_ip_assign_failed", error=str(exc))
            return False

    def _systemctl(self, action: str, unit: str) -> bool:
        """Thin wrapper around `systemctl <action> <unit>`."""
        try:
            result = subprocess.run(
                ["systemctl", action, unit],
                check=False,
                capture_output=True,
                timeout=10,
            )
            if result.returncode != 0:
                log.warning(
                    "systemctl_nonzero",
                    action=action,
                    unit=unit,
                    rc=result.returncode,
                    stderr=result.stderr.decode(errors="replace").strip(),
                )
                return False
            return True
        except (OSError, subprocess.SubprocessError) as exc:
            log.warning(
                "systemctl_failed", action=action, unit=unit, error=str(exc)
            )
            return False

    def start(self) -> bool:
        """Bring the AP up: write configs, assign IP, start units."""
        if os.geteuid() != 0:
            log.warning(
                "hostapd_start_non_root",
                msg="AP operations require root, continuing anyway",
            )

        self.write_config()
        self._assign_ip()

        hostapd_ok = self._systemctl("start", _HOSTAPD_UNIT)
        dnsmasq_ok = self._systemctl("start", _DNSMASQ_UNIT)

        self._running = hostapd_ok
        log.info(
            "ap_started",
            hostapd=hostapd_ok,
            dnsmasq=dnsmasq_ok,
            ssid=self._ssid,
        )
        return hostapd_ok

    def stop(self) -> None:
        """Tear the AP down. Best-effort on both units."""
        self._systemctl("stop", _DNSMASQ_UNIT)
        self._systemctl("stop", _HOSTAPD_UNIT)
        self._running = False
        log.info("ap_stopped")

    def _is_unit_active(self, unit: str) -> bool:
        try:
            result = subprocess.run(
                ["systemctl", "is-active", unit],
                check=False,
                capture_output=True,
                timeout=5,
            )
            return result.stdout.decode(errors="replace").strip() == "active"
        except (OSError, subprocess.SubprocessError):
            return False

    def _connected_clients(self) -> list[str]:
        """Scrape `iw dev wlan0 station dump` for associated MAC addresses."""
        try:
            result = subprocess.run(
                ["iw", "dev", self._interface, "station", "dump"],
                check=False,
                capture_output=True,
                timeout=5,
            )
        except (OSError, subprocess.SubprocessError) as exc:
            log.debug("iw_station_dump_failed", error=str(exc))
            return []

        if result.returncode != 0:
            return []

        text = result.stdout.decode(errors="replace")
        macs: list[str] = []
        for line in text.splitlines():
            line = line.strip()
            if line.startswith("Station "):
                parts = line.split()
                if len(parts) >= 2:
                    macs.append(parts[1].lower())
        return macs

    def status(self) -> dict:
        """Return live status for the AP."""
        running = self._is_unit_active(_HOSTAPD_UNIT)
        clients = self._connected_clients() if running else []
        return {
            "running": running,
            "ssid": self._ssid,
            "channel": self._channel,
            "interface": self._interface,
            "gateway": _AP_ADDR,
            "connected_clients": clients,
        }

    def apply_ap_config(
        self,
        ssid: str | None,
        passphrase: str | None,
        channel: int | None,
    ) -> bool:
        """Idempotent update. Restarts hostapd only if something changed.

        Any of the three arguments may be None to leave that field
        unchanged. Passphrase updates overwrite `/etc/ados/ap-passphrase`.
        """
        changed = False

        if ssid is not None and ssid != self._ssid:
            self._ssid = ssid
            changed = True

        if channel is not None and channel != self._channel:
            self._channel = channel
            changed = True

        if passphrase is not None and passphrase != self._passphrase:
            self._passphrase = passphrase
            try:
                _PASSPHRASE_PATH.parent.mkdir(parents=True, exist_ok=True)
                _PASSPHRASE_PATH.write_text(passphrase + "\n", encoding="utf-8")
                os.chmod(_PASSPHRASE_PATH, 0o600)
            except OSError as exc:
                log.error("ap_passphrase_update_failed", error=str(exc))
                return False
            changed = True

        if not changed:
            log.debug("ap_config_unchanged")
            return True

        self.write_config()
        # Restart is safer than reload for SSID/channel changes.
        self._systemctl("restart", _HOSTAPD_UNIT)
        log.info(
            "ap_config_applied",
            ssid=self._ssid,
            channel=self._channel,
        )
        return True


async def main() -> None:
    """Service entry point. Invoked by systemd via `python -m`."""
    config = load_config()
    configure_logging(config.logging.level)
    slog = structlog.get_logger()
    slog.info("ground_hostapd_service_starting")

    device_id = config.agent.device_id
    hotspot = config.network.hotspot

    # If the user set a literal SSID in config (no template), honor it.
    ssid_override: str | None = None
    if hotspot.ssid and "{device_id}" not in hotspot.ssid and hotspot.ssid.strip():
        if hotspot.ssid.startswith("ADOS-GS-"):
            ssid_override = hotspot.ssid

    manager = HostapdManager(
        device_id=device_id,
        ssid=ssid_override,
        channel=hotspot.channel,
    )
    manager.ensure_passphrase()

    ok = manager.start()
    if not ok:
        slog.error("ground_hostapd_start_failed")
        sys.exit(2)

    slog.info(
        "ground_hostapd_service_ready",
        ssid=manager.ssid,
        channel=manager.channel,
    )

    shutdown = asyncio.Event()
    loop = asyncio.get_event_loop()
    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, shutdown.set)

    await shutdown.wait()

    slog.info("ground_hostapd_service_stopping")
    manager.stop()
    slog.info("ground_hostapd_service_stopped")


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
    sys.exit(0)
