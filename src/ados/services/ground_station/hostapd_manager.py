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
import json
import os
import re
import secrets
import signal
import sys
from pathlib import Path
from typing import TYPE_CHECKING, Any

import structlog

if TYPE_CHECKING:
    from ados.services.ground_station.mdns_announce import APAnnouncer

from ados.core.config import load_config
from ados.core.logging import configure_logging, get_logger
from ados.core.paths import (
    AP_PASSPHRASE_PATH,
    DNSMASQ_CONF_PATH,
    HOSTAPD_CONF_PATH,
)
from ados.core.subprocess import CmdTimeout, run_cmd_sync

log = get_logger("ground_station.hostapd")

# Honored for legacy installs and for operators who explicitly rotated the
# passphrase by writing this file. New installs rely on
# ``network.hotspot.password`` from config; the agent never auto-generates.
_PASSPHRASE_PATH = AP_PASSPHRASE_PATH


# Characters an operator can read off a screen and type without guessing:
# 0/O and 1/I/L are excluded. Mirrors the Rust `UNAMBIGUOUS_CHARSET`.
_UNAMBIGUOUS_CHARSET = "ABCDEFGHJKMNPQRSTUVWXYZ23456789"

# WPA2-PSK accepts 8..63 printable ASCII. Twelve from a 31-character alphabet
# is about 59 bits, and still short enough to read off a small display and type
# into a phone. Mirrors the Rust `AP_PASSPHRASE_LEN`.
_AP_PASSPHRASE_LEN = 12


# Country advertised when the operator has pinned no region. Matches the radio
# reconciler's own default so the two halves of a stock box agree; it was a
# hardcoded "IN" while the radio defaulted to "US", so a stock box declared two
# different jurisdictions at once. Mirrors the Rust `DEFAULT_AP_COUNTRY`.
_DEFAULT_AP_COUNTRY = "US"


def _resolve_ap_country(config_path: str = "/etc/ados/config.yaml") -> str:
    """The country hostapd should advertise, from the operator's pinned region.

    The region counts only when the operator has actually opted into a
    jurisdiction (``mode: region``), mirroring how the radio reads it. Anything
    unusable falls back rather than reaching hostapd, which refuses to start on
    a bad country code and so takes the access point down rather than
    degrading it.
    """
    try:
        import yaml

        with open(config_path, encoding="utf-8") as fh:
            raw = yaml.safe_load(fh) or {}
    except (OSError, ValueError, ImportError):
        return _DEFAULT_AP_COUNTRY
    if not isinstance(raw, dict):
        return _DEFAULT_AP_COUNTRY
    reg = ((raw.get("network") or {}).get("regulatory")) or {}
    if not isinstance(reg, dict):
        return _DEFAULT_AP_COUNTRY
    if str(reg.get("mode") or "").strip().lower() != "region":
        return _DEFAULT_AP_COUNTRY
    region = str(reg.get("region") or "").strip().upper()
    if len(region) == 2 and region.isalpha():
        return region
    return _DEFAULT_AP_COUNTRY


def read_ap_passphrase() -> str:
    """The persisted AP passphrase, or empty when there is none.

    Read-only on purpose. `ensure_passphrase` GENERATES when the file is
    absent, so calling it from a status path made a GET create and persist a
    secret as a side effect — and, before the create was made exclusive, a
    different one from the value hostapd had loaded.
    """
    try:
        return _PASSPHRASE_PATH.read_text(encoding="utf-8").strip()
    except OSError:
        return ""


def generate_ap_passphrase() -> str:
    """Draw a fresh per-unit AP passphrase, legal for WPA2-PSK.

    `secrets.choice` is uniform over the sequence and raises rather than
    degrading if the system has no usable entropy source, which is the
    fail-closed behaviour every other secret this agent draws uses.
    """
    return "".join(
        secrets.choice(_UNAMBIGUOUS_CHARSET) for _ in range(_AP_PASSPHRASE_LEN)
    )

_HOSTAPD_CONF_PATH = HOSTAPD_CONF_PATH
_DNSMASQ_CONF_PATH = DNSMASQ_CONF_PATH

_AP_IFACE = "wlan0"


def _radio_interface(run_root: Path | None = None) -> str:
    """The interface the radio reports it actually opened, or "".

    Driver classification is a guess made from a generated table; this is the
    radio's own account of what it took. It is the authority when the two
    disagree, because a table can be wrong and the radio cannot be wrong about
    the device it opened. Mirrors ``ados_protocol::netif::radio_interface``.
    """
    path = (run_root or Path("/run/ados")) / "wfb-stats.json"
    try:
        stats = json.loads(path.read_text())
    except (OSError, ValueError):
        return ""
    iface = stats.get("interface") if isinstance(stats, dict) else None
    return iface.strip() if isinstance(iface, str) else ""


def resolve_ap_interface(
    configured: str = "",
    fallback: str = _AP_IFACE,
    net_root: Path | None = None,
    run_root: Path | None = None,
) -> str:
    """Return the interface the access point should bind, resolved by DRIVER.

    Interface names are not stable. Measured across three reboots of a ground
    station, ``wlan0`` was the onboard chip twice and the USB WFB flight radio
    once -- so binding the AP to a name meant a one-in-three chance of running
    hostapd on the aircraft's radio link.

    Classification uses the generated deny-set the radio itself consults to make
    sure it never grabs management WiFi for injection. Read the other way round,
    that set is exactly "the interface the access point wants".

    The radio's own sidecar overrides that classification wherever the two
    disagree. This is the ground station's live resolver, so the backstop has to
    exist here and not only in the Rust twin: a misclassified flight radio that
    the table failed to recognise is exactly the case a driver table cannot
    catch, and the cost of getting it wrong is hostapd on the aircraft's link.

    Mirrors ``EthernetManager``/``UplinkRouter``, which already resolve their NIC
    at construction time for this same udev-race reason, and the Rust twin in
    ``ados_protocol::netif``. Falls back to the previous constant rather than
    raising: the caller's start path refuses separately if the fallback turns
    out to be the radio.
    """
    from ados.services.network.interface_roles import (
        driver_of as _driver_of,
    )
    from ados.services.network.interface_roles import (
        is_denied_management_driver,
        is_wfb_compatible_driver,
        wireless_interfaces,
    )

    root = net_root or Path("/sys/class/net")
    radio_iface = _radio_interface(run_root)

    def driver_of(iface: str) -> str:
        return _driver_of(iface, root)

    def is_radio(iface: str) -> bool:
        if radio_iface and iface == radio_iface:
            return True
        return is_wfb_compatible_driver(driver_of(iface))

    def is_onboard(iface: str) -> bool:
        # The radio's own account outranks the deny-prefix table: an interface
        # the radio says it holds is never a candidate, whatever its driver
        # string looks like.
        if radio_iface and iface == radio_iface:
            return False
        return is_denied_management_driver(driver_of(iface))

    wireless = wireless_interfaces(root)

    configured = configured.strip()
    if configured:
        if configured in wireless and is_radio(configured):
            log.error(
                "ap_interface_configured_is_the_wfb_radio",
                interface=configured,
                radio_interface=radio_iface or None,
            )
            return fallback
        return configured

    for iface in wireless:
        if is_onboard(iface):
            return iface

    log.error("ap_interface_no_onboard_wifi", candidates=wireless)
    return fallback


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
        passphrase: str = "",
    ) -> None:
        self._device_id = device_id
        self._ssid = ssid or _build_ssid(device_id)
        self._channel = channel
        self._interface = interface
        self._configured_passphrase = passphrase
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
        """Resolve the AP passphrase to use.

        Order of precedence:
        1. ``/etc/ados/ap-passphrase`` if it exists. Honored for
           legacy installs and for operators who explicitly rotated the
           passphrase by writing the file.
        2. The configured ``network.hotspot.password`` passed into the
           manager constructor, when an operator has set one in
           ``/etc/ados/config.yaml``.
        3. A freshly generated per-unit passphrase.

        Step 3 used to be a single built-in string shared by every unit
        ever shipped. One published default on every access point is not
        a secret: anyone within radio range of any ADOS ground station
        could join the network of any other.

        Generating is only safe because the value is now displayed — on
        the installer's completion card and in the on-box status view.
        Nothing showed it before, so a generated passphrase would have
        been undiscoverable and the unit unjoinable. If that display
        path is removed, this has to go back with it.
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

        configured = (self._configured_passphrase or "").strip()
        if configured:
            self._passphrase = configured
            log.info("ap_passphrase_from_config")
            return self._passphrase

        try:
            self._passphrase = generate_ap_passphrase()
            # Persist immediately. A generated value that is not written is a
            # DIFFERENT passphrase on every restart: the operator reads one off
            # the installer card, the service restarts, and the network they
            # were told to join no longer exists. The file is also what makes
            # the first branch above win next time, so without this the value
            # is never stable.
            try:
                _PASSPHRASE_PATH.parent.mkdir(parents=True, exist_ok=True)
                # Create EXCLUSIVELY at 0600 in one syscall, and adopt the
                # winner on a collision. Several processes resolve this on a
                # fresh boot; each used to draw its own value and write the
                # file, so the value an operator was shown could differ from
                # the one hostapd loaded. The separate write-then-chmod also
                # left a world-readable window.
                fd = os.open(
                    _PASSPHRASE_PATH,
                    os.O_WRONLY | os.O_CREAT | os.O_EXCL,
                    0o600,
                )
                with os.fdopen(fd, "w", encoding="utf-8") as fh:
                    fh.write(self._passphrase + "\n")
                log.info("ap_passphrase_generated", path=str(_PASSPHRASE_PATH))
            except FileExistsError:
                existing = read_ap_passphrase()
                if existing:
                    self._passphrase = existing
                    log.info("ap_passphrase_adopted_from_concurrent_writer")
                else:
                    log.warning("ap_passphrase_race_left_an_unreadable_file")
            except OSError as exc:
                log.error(
                    "ap_passphrase_generated_but_not_persisted",
                    path=str(_PASSPHRASE_PATH),
                    error=str(exc),
                )
        except OSError as exc:
            # Actually fail closed. This used to substitute a single passphrase
            # compiled into every unit, while the comment above it claimed to be
            # failing closed.
            #
            # One published string shared by every ground station ever shipped
            # is worse than having no access point: the network presents as
            # protected, so nobody knows to distrust it. An empty passphrase
            # stops the config write, so the AP simply does not come up.
            log.error(
                "ap_passphrase_generate_failed_refusing_to_start_ap",
                error=str(exc),
            )
            self._passphrase = ""
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
            f"country_code={_resolve_ap_country()}",
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

        # Still empty means the RNG failed and there is no passphrase to use.
        # Refuse here rather than emitting a conf: WPA requires 8-63 characters,
        # so an empty one either yields an open network or a start-time failure
        # from hostapd that reads as an unrelated fault.
        if not self._passphrase:
            log.error("ap_config_refused_no_passphrase")
            raise OSError("refusing to write hostapd.conf without a passphrase")

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
            run_cmd_sync(
                ["ip", "addr", "add", _AP_CIDR, "dev", self._interface],
                timeout=5.0,
            )
            run_cmd_sync(
                ["ip", "link", "set", self._interface, "up"],
                timeout=5.0,
            )
            return True
        except (OSError, CmdTimeout) as exc:
            log.warning("ap_ip_assign_failed", error=str(exc))
            return False

    def _systemctl(self, action: str, unit: str) -> bool:
        """Thin wrapper around `systemctl <action> <unit>`."""
        try:
            result = run_cmd_sync(
                ["systemctl", action, unit],
                timeout=10.0,
            )
            if not result.ok:
                log.warning(
                    "systemctl_nonzero",
                    action=action,
                    unit=unit,
                    rc=result.returncode,
                    stderr=result.stderr.strip(),
                )
                return False
            return True
        except (OSError, CmdTimeout) as exc:
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
            result = run_cmd_sync(
                ["systemctl", "is-active", unit],
                timeout=5.0,
            )
            return result.stdout.strip() == "active"
        except (OSError, CmdTimeout):
            return False

    def _connected_clients(self) -> list[str]:
        """Scrape `iw dev wlan0 station dump` for associated MAC addresses."""
        try:
            result = run_cmd_sync(
                ["iw", "dev", self._interface, "station", "dump"],
                timeout=5.0,
            )
        except (OSError, CmdTimeout) as exc:
            log.debug("iw_station_dump_failed", error=str(exc))
            return []

        if not result.ok:
            return []

        text = result.stdout
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


async def _run_ap_announcer(
    announcer: APAnnouncer,
    shutdown: asyncio.Event,
    slog: Any,
    initial_delay: float = 2.0,
    retry_interval: float = 5.0,
) -> None:
    """Background task: keep an mDNS announcement alive while the AP is up.

    Polls the wlan0 address until it matches the expected AP IP, then
    registers the service. If the IP disappears later (interface flap,
    operator turning the AP off), unregisters and waits for it to come
    back. Runs until `shutdown` is set.
    """
    await asyncio.sleep(initial_delay)
    registered = False
    while not shutdown.is_set():
        ap_up = announcer.is_ap_up()
        if ap_up and not registered:
            registered = announcer.start()
            if not registered:
                slog.warning("ap_announce_start_failed_will_retry")
        elif not ap_up and registered:
            announcer.stop()
            registered = False
            slog.info("ap_announce_paused_iface_down")
        try:
            await asyncio.wait_for(shutdown.wait(), timeout=retry_interval)
        except TimeoutError:
            continue
    if registered:
        announcer.stop()


async def main() -> None:
    """Service entry point. Invoked by systemd via `python -m`."""
    from ados import __version__ as agent_version
    from ados.services.ground_station.mdns_announce import APAnnouncer

    config = load_config()
    configure_logging(config.logging.level)
    slog = structlog.get_logger()
    slog.info("ground_hostapd_service_starting")

    device_id = config.agent.device_id
    hotspot = config.network.hotspot

    # Opt-in gate. The hotspot is off by default; operators who want
    # it enable it via the Setup webapp Network step or by writing
    # network.hotspot.enabled=true into /etc/ados/config.yaml. Without
    # this gate the systemd unit would attempt to bind hostapd to
    # wlan0 even when the operator left the field on the default
    # false, which on a box that's already a WiFi client gives the
    # interface two IPs (DHCP + 192.168.4.1) and tends to break the
    # home-WiFi association.
    #
    # Idle-sleep (not exit) so systemd keeps the unit in `active` state
    # and the supervisor's monitor loop doesn't see a Type=simple
    # process exit as `service_died` and start retrying. The operator
    # restarts the unit after toggling hotspot.enabled=true via the
    # Setup webapp; on the next start the idle branch is skipped.
    if not hotspot.enabled:
        slog.info(
            "hotspot_disabled_by_config",
            note="operator opt-in not set; idling. Toggle via Setup webapp to activate.",
        )
        # Park forever; systemd considers the service active.
        while True:
            await asyncio.sleep(3600)

    # If the user set a literal SSID in config (no template), honor it.
    ssid_override: str | None = None
    if hotspot.ssid and "{device_id}" not in hotspot.ssid and hotspot.ssid.strip():
        if hotspot.ssid.startswith("ADOS-GS-"):
            ssid_override = hotspot.ssid

    manager = HostapdManager(
        device_id=device_id,
        ssid=ssid_override,
        channel=hotspot.channel,
        passphrase=hotspot.password,
        # Resolved here rather than defaulted: the interface names race at boot,
        # so "wlan0" is right only about two boots in three on this hardware.
        interface=resolve_ap_interface(hotspot.interface),
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

    # Advertise the agent REST/WS surface on the AP so the Android
    # client and any other LAN consumer can discover the endpoint
    # without hardcoding the IP. Announcement lifecycle follows wlan0.
    announcer = APAnnouncer(
        port=8080,
        device_id=device_id,
        version=agent_version,
        iface=manager.interface,
    )
    announcer_task = asyncio.create_task(
        _run_ap_announcer(announcer, shutdown, slog)
    )

    await shutdown.wait()

    slog.info("ground_hostapd_service_stopping")
    announcer_task.cancel()
    try:
        await announcer_task
    except asyncio.CancelledError:
        pass
    except Exception:
        pass
    manager.stop()
    slog.info("ground_hostapd_service_stopped")


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
    sys.exit(0)
