"""Network / host helpers: hostname, local IPs, MAVLink port pickers, host validation."""

from __future__ import annotations

import socket
import subprocess
from pathlib import Path
from typing import Any

from ._constants import (
    _HOTSPOT_IP,
    _USB_GADGET_IP,
    DEFAULT_MAVLINK_TCP_PORT,
)

# Priority order for the single ``uplink_kind`` field surfaced to the
# Connectivity tile. Mirrors what an operator cares about first: a wired
# link beats wireless, and a real WiFi association beats a USB tether
# fallback. Cellular is last because the modem probe is best-effort.
_UPLINK_PRIORITY = ("ethernet", "wifi", "usb-tether", "cellular")


def _hostname() -> str:
    try:
        return socket.gethostname()
    except OSError:
        return ""


def _local_ips() -> list[str]:
    ips: set[str] = set()
    try:
        import psutil  # type: ignore[import-untyped]

        for addrs in psutil.net_if_addrs().values():
            for addr in addrs:
                if getattr(addr, "family", None) == socket.AF_INET:
                    value = str(getattr(addr, "address", ""))
                    if value and not value.startswith("127."):
                        ips.add(value)
    except Exception:
        pass

    if not ips:
        try:
            with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
                sock.connect(("8.8.8.8", 80))
                ips.add(sock.getsockname()[0])
        except OSError:
            pass

    return sorted(ips)


def _local_ip_addresses() -> dict[str, str]:
    """Return a {iface: ipv4} map for every non-loopback IPv4 the host owns.

    Same data source as :func:`_local_ips` but keyed so the Connectivity
    tile can surface ``end0 = 192.168.1.42`` instead of an opaque blob.
    """
    addresses: dict[str, str] = {}
    try:
        import psutil  # type: ignore[import-untyped]

        for iface, addrs in psutil.net_if_addrs().items():
            for addr in addrs:
                if getattr(addr, "family", None) != socket.AF_INET:
                    continue
                value = str(getattr(addr, "address", ""))
                if value and not value.startswith("127."):
                    addresses[str(iface)] = value
                    break
    except Exception:
        pass
    return addresses


def _probe_active_uplink_kind() -> str | None:
    """Single-value uplink_kind for the Connectivity tile.

    Re-uses :func:`ados.bootstrap.profile_detect.probe_uplink_kinds` plus
    the ModemManager probe behind ``_check_uplink`` and reduces the multi
    answer to one ranked value. Returns ``None`` when nothing is up so the
    frontend can render the warn-state without inventing a string.
    """
    detected: list[str] = []
    try:
        from ados.bootstrap.profile_detect import probe_uplink_kinds

        detected.extend(probe_uplink_kinds())
    except Exception:
        pass
    try:
        from ados.hal.modem import detect_modem

        modem = detect_modem()
        if modem and str(getattr(modem, "connection_state", "")).lower() in (
            "connected",
            "registered",
        ):
            detected.append("cellular")
    except Exception:
        pass

    normalized = []
    for kind in detected:
        token = kind.strip().lower()
        if token == "wifi" or token == "wi-fi":
            normalized.append("wifi")
        elif token == "usb tether" or token == "usb-tether":
            normalized.append("usb-tether")
        elif token == "4g" or token == "cellular":
            normalized.append("cellular")
        elif token:
            normalized.append(token)

    for preferred in _UPLINK_PRIORITY:
        if preferred in normalized:
            return preferred
    return normalized[0] if normalized else None


def _probe_wifi_ssid() -> str | None:
    """Best-effort SSID lookup for an associated wlan interface.

    Tries ``iwgetid`` first (`wireless-tools`, almost always present on
    Linux), falls back to parsing ``iw dev <iface> link`` for any wlan
    interface in ``/sys/class/net``. Returns ``None`` when no association
    is found or neither tool is available.
    """
    try:
        result = subprocess.run(
            ["iwgetid", "-r"],
            capture_output=True,
            text=True,
            timeout=1.5,
            check=False,
        )
        ssid = (result.stdout or "").strip()
        if ssid:
            return ssid
    except (FileNotFoundError, subprocess.TimeoutExpired, OSError):
        pass

    try:
        for wlan_dir in sorted(Path("/sys/class/net").glob("wlan*")):
            iface = wlan_dir.name
            try:
                link = subprocess.run(
                    ["iw", "dev", iface, "link"],
                    capture_output=True,
                    text=True,
                    timeout=1.5,
                    check=False,
                )
            except (FileNotFoundError, subprocess.TimeoutExpired, OSError):
                continue
            for line in (link.stdout or "").splitlines():
                line = line.strip()
                if line.lower().startswith("ssid:"):
                    candidate = line.split(":", 1)[1].strip()
                    if candidate:
                        return candidate
    except OSError:
        pass
    return None


def _probe_wifi_rssi_dbm() -> int | None:
    """Best-effort RSSI in dBm from ``/proc/net/wireless`` (column 4).

    Returns ``None`` when no associated wlan is found. Values surfaced by
    the kernel are already in dBm for modern drivers, so a positive
    intermediate value (older noise-level encoding) is rejected to avoid
    rendering garbage to the operator.
    """
    try:
        text = Path("/proc/net/wireless").read_text()
    except OSError:
        return None
    for line in text.splitlines()[2:]:
        parts = line.split()
        if len(parts) < 4:
            continue
        try:
            value = float(parts[3].rstrip("."))
        except ValueError:
            continue
        if -120.0 <= value <= -10.0:
            return int(value)
    return None


def _first_mavlink_ws_port(config: Any) -> int:
    for endpoint in getattr(config.mavlink, "endpoints", []):
        if getattr(endpoint, "type", "") == "websocket" and getattr(endpoint, "enabled", False):
            return int(getattr(endpoint, "port", 8765))
    return 8765


def _first_mavlink_tcp_port(config: Any) -> int | None:
    """Return the MAVLink TCP server port the agent serves on.

    Mirrors ``_first_mavlink_ws_port`` but for the desktop-GCS-friendly
    TCP listener. Walks ``config.mavlink.endpoints`` first so an operator
    who explicitly disabled the listener (or moved it to a non-default
    port) wins. Falls back to ``DEFAULT_MAVLINK_TCP_PORT`` since the
    in-process TCP proxy is started unconditionally with that port.
    """
    found_disabled = False
    for endpoint in getattr(config.mavlink, "endpoints", []):
        etype = str(getattr(endpoint, "type", "") or "")
        if etype in ("tcp", "tcp_server"):
            if getattr(endpoint, "enabled", False):
                return int(getattr(endpoint, "port", DEFAULT_MAVLINK_TCP_PORT))
            found_disabled = True
    if found_disabled:
        # Operator explicitly disabled the TCP listener via config —
        # don't advertise it.
        return None
    return DEFAULT_MAVLINK_TCP_PORT


def _best_lan_host(hostname: str, local_ips: list[str]) -> str:
    """Pick the most operator-friendly LAN-routable host string.

    Preference order:
    1. ``<hostname>.local`` when the system hostname looks routable.
       ``groundnode`` becomes ``groundnode.local`` — the form a bench
       operator already typed at the SSH prompt.
    2. ``<hostname>`` itself when it already carries a dot (admin set a
       full DNS name).
    3. First non-loopback IPv4 from the discovered set.
    4. Empty string when nothing is reachable from the LAN.
    """
    name = (hostname or "").strip().rstrip(".")
    if name and name not in ("", "localhost") and not name.startswith("127."):
        if "." in name:
            return name
        return f"{name}.local"
    for ip in local_ips:
        if ip and not ip.startswith("127."):
            return ip
    return ""


def _build_known_hosts(
    *,
    local_ips: list[str],
    mdns_host: str,
    config: Any,
) -> set[str]:
    """The set of host strings the agent will accept in a Host header.

    Used to reject Host-header injection from a hostile upstream proxy. We
    accept localhost, the configured mDNS host, every discovered local IP,
    and the hotspot/USB-gadget addresses the agent itself binds.
    """
    hosts: set[str] = {"localhost", "127.0.0.1", _HOTSPOT_IP, _USB_GADGET_IP}
    if mdns_host:
        hosts.add(mdns_host)
    hostname = _hostname()
    if hostname:
        hosts.add(hostname)
        hosts.add(f"{hostname}.local")
    for ip in local_ips:
        hosts.add(ip)
    cf = getattr(config, "remote_access", None)
    if cf is not None:
        cloudflare = getattr(cf, "cloudflare", None)
        for url in (
            getattr(cloudflare, "setup_url", "") if cloudflare else "",
            getattr(cloudflare, "api_url", "") if cloudflare else "",
        ):
            if url:
                try:
                    parsed_host = url.split("://", 1)[-1].split("/", 1)[0].split(":", 1)[0]
                    if parsed_host:
                        hosts.add(parsed_host)
                except Exception:
                    pass
    return hosts


def _safe_host_for(host_header: str | None, known_hosts: set[str]) -> str:
    """Validate a Host header against known-good hosts.

    Returns ``host:port`` when the header carries a host the agent itself
    advertises; otherwise falls back to ``localhost:8080``. Multi-value
    chains (proxy lists) take only the first entry.
    """
    if not host_header:
        return "localhost:8080"
    candidate = host_header.split(",")[0].strip()
    if not candidate:
        return "localhost:8080"
    host_only = candidate.split(":", 1)[0]
    if host_only and host_only in known_hosts:
        return candidate
    return "localhost:8080"


__all__ = [
    "_hostname",
    "_local_ips",
    "_local_ip_addresses",
    "_probe_active_uplink_kind",
    "_probe_wifi_ssid",
    "_probe_wifi_rssi_dbm",
    "_first_mavlink_ws_port",
    "_first_mavlink_tcp_port",
    "_best_lan_host",
    "_build_known_hosts",
    "_safe_host_for",
]
