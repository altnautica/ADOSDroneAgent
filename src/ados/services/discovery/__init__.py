"""mDNS discovery service using zeroconf."""

from __future__ import annotations

import socket
import subprocess
import time
from typing import TYPE_CHECKING

from ados.core.logging import get_logger

if TYPE_CHECKING:
    from zeroconf.asyncio import AsyncServiceInfo

log = get_logger("discovery")

SERVICE_TYPE = "_ados._tcp.local."

# How long one avahi host-name probe is reused, matching the Rust reach rule.
_AVAHI_PROBE_TTL_S = 30.0


def _normalize_hostname(raw: str) -> str | None:
    """Trim a raw hostname; ``None`` for one that cannot be another machine's reach."""
    name = raw.strip().rstrip(".").strip()
    if not name:
        return None
    if name.lower() in ("localhost", "localhost.localdomain"):
        return None
    if name.startswith("127."):
        return None
    return name


def _mdns_name_from(hostname: str) -> str:
    """The first label of ``hostname`` under ``.local``: what an mDNS responder publishes."""
    return f"{hostname.split('.', 1)[0]}.local"


def _parse_busctl_string(reply: str) -> str | None:
    """The name in a ``busctl call`` string reply, kept only when it is a ``.local`` name."""
    quoted = reply.strip()
    if not quoted.startswith("s "):
        return None
    quoted = quoted[2:].strip()
    if len(quoted) < 2 or not (quoted.startswith('"') and quoted.endswith('"')):
        return None
    name = _normalize_hostname(quoted[1:-1])
    if name is None or not name.lower().endswith(".local"):
        return None
    return name


def _avahi_host_fqdn() -> str | None:
    """avahi's published host name over D-Bus, or ``None`` when avahi is not answering."""
    try:
        out = subprocess.run(
            [
                "busctl",
                "--system",
                "--timeout=1",
                "call",
                "org.freedesktop.Avahi",
                "/",
                "org.freedesktop.Avahi.Server",
                "GetHostNameFqdn",
            ],
            capture_output=True,
            text=True,
            timeout=2,
            check=False,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if out.returncode != 0:
        return None
    return _parse_busctl_string(out.stdout)


class DiscoveryService:
    """Registers and manages mDNS service for local network discovery."""

    def __init__(
        self,
        device_id: str,
        port: int = 8080,
        name: str = "my-drone",
        version: str = "0.2.0",
        board: str = "unknown",
    ):
        self._device_id = device_id
        self._port = port
        self._name = name
        self._version = version
        self._board = board
        self._zeroconf = None
        self._info: AsyncServiceInfo | None = None
        self._short_id = device_id[:6].lower()
        self._avahi_probe: tuple[float, str | None] | None = None

    def _published_name(self) -> str | None:
        """avahi's published name, probed at most once per ``_AVAHI_PROBE_TTL_S``."""
        now = time.monotonic()
        if self._avahi_probe is not None and now - self._avahi_probe[0] < _AVAHI_PROBE_TTL_S:
            return self._avahi_probe[1]
        name = _avahi_host_fqdn()
        self._avahi_probe = (now, name)
        return name

    @property
    def mdns_hostname(self) -> str:
        # The same rule as the native front's reach name: avahi's published
        # host name wins (it follows a collision rename to `<host>-2.local`);
        # otherwise the system hostname's first label under `.local`, which is
        # what an mDNS responder publishes. A constructed `ados-<short_id>.local`
        # is used only when the host has no usable hostname at all.
        published = self._published_name()
        if published is not None:
            return published
        name = _normalize_hostname(socket.gethostname() or "")
        if name is not None:
            return _mdns_name_from(name)
        return f"ados-{self._short_id}.local"

    def _local_addresses(self) -> list[str]:
        """Every IPv4 address on this node's interfaces, loopback excluded.

        Read from the interfaces themselves rather than from the route to a
        public host: an AP-only ground station has no default route, yet its
        AP address is exactly what a phone on that AP must be handed. A
        loopback address is never advertised; it would send a remote client
        to itself.
        """
        import ipaddress

        import ifaddr

        out: list[str] = []
        for adapter in ifaddr.get_adapters():
            for ip in adapter.ips:
                if not isinstance(ip.ip, str):
                    continue  # IPv6 entries carry a tuple
                try:
                    addr = ipaddress.IPv4Address(ip.ip)
                except ValueError:
                    continue
                if addr.is_loopback or addr.is_unspecified:
                    continue
                if ip.ip not in out:
                    out.append(ip.ip)
        return out

    def _build_txt_records(
        self,
        paired: bool = False,
        code: str | None = None,
        owner: str | None = None,
        profile: str | None = None,
        role: str | None = None,
    ) -> dict:
        records = {
            "device_id": self._device_id,
            "version": self._version,
            "board": self._board,
            "name": self._name,
            "paired": str(paired).lower(),
        }
        if code and not paired:
            records["code"] = code
        if owner and paired:
            records["owner"] = owner
        if profile:
            records["profile"] = profile
        if role:
            records["role"] = role
        return records

    def _build_info(self, addresses: list[str], txt_records: dict[str, str]) -> AsyncServiceInfo:
        """The service record, for both the first registration and every refresh.

        The SRV target must be a name that resolves. Publishing
        ``server="ados-<short_id>.local."`` creates no matching A-record —
        avahi publishes exactly one resolvable ``<hostname>.local``, the system
        hostname — so a browser that follows the SRV name instead of the
        attached address gets a lookup failure. Use the reach name
        ``mdns_hostname`` reports.
        """
        from zeroconf.asyncio import AsyncServiceInfo

        return AsyncServiceInfo(
            SERVICE_TYPE,
            f"ADOS-{self._short_id}.{SERVICE_TYPE}",
            addresses=[socket.inet_aton(ip) for ip in addresses],
            port=self._port,
            properties=txt_records,
            server=f"{self.mdns_hostname}.",
        )

    @property
    def registered(self) -> bool:
        return self._zeroconf is not None and self._info is not None

    async def register(
        self,
        paired: bool = False,
        code: str | None = None,
        owner: str | None = None,
        profile: str | None = None,
        role: str | None = None,
    ) -> bool:
        """Register mDNS service on the local network; ``True`` once registered.

        A node with no usable address yet (network still coming up) is left
        unregistered and the caller retries; nothing is advertised meanwhile.
        """
        addresses = self._local_addresses()
        if not addresses:
            log.info("discovery_waiting_for_address")
            return False
        zc = None
        try:
            from zeroconf import IPVersion
            from zeroconf.asyncio import AsyncZeroconf

            info = self._build_info(
                addresses, self._build_txt_records(paired, code, owner, profile, role)
            )
            zc = AsyncZeroconf(ip_version=IPVersion.V4Only)
            await zc.async_register_service(info)
        except Exception as e:
            log.warning("discovery_register_failed", error=str(e))
            if zc is not None:
                try:
                    await zc.async_close()
                except Exception:  # noqa: BLE001 — already failing; retry later
                    pass
            return False
        self._zeroconf = zc
        self._info = info
        log.info(
            "discovery_registered",
            service=info.name,
            addresses=addresses,
            port=self._port,
            hostname=self.mdns_hostname,
        )
        return True

    async def refresh(
        self,
        paired: bool = False,
        code: str | None = None,
        owner: str | None = None,
        profile: str | None = None,
        role: str | None = None,
    ) -> bool:
        """Register if not yet registered, else update TXT and addresses.

        Returns whether the service is registered afterwards.
        """
        if not self.registered:
            return await self.register(paired, code, owner, profile, role)
        addresses = self._local_addresses()
        if not addresses:
            # Every address went away: stop advertising rather than pointing
            # clients at an address the node no longer holds.
            await self.unregister()
            return False
        try:
            new_info = self._build_info(
                addresses, self._build_txt_records(paired, code, owner, profile, role)
            )
            await self._zeroconf.async_update_service(new_info)
            self._info = new_info
            log.debug("discovery_txt_updated", paired=paired)
        except Exception as e:
            log.warning("discovery_update_failed", error=str(e))
        return True

    async def unregister(self) -> None:
        """Unregister mDNS service."""
        if self._zeroconf:
            try:
                if self._info:
                    unregister_task = await self._zeroconf.async_unregister_service(
                        self._info
                    )
                    await unregister_task
                await self._zeroconf.async_close()
                log.info("discovery_unregistered")
            except Exception as e:
                log.warning("discovery_unregister_failed", error=str(e))
            finally:
                self._zeroconf = None
                self._info = None
