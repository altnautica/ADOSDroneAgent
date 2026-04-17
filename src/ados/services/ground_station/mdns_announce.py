"""mDNS announce/resolve helpers scoped to the mesh interface (MSN-035).

Receivers publish `_ados-receiver._tcp` on the batman-adv interface so
relays can find them without shared infra. This module isolates the
zeroconf + interface-scoping logic so `wfb_relay`, `wfb_receiver`, and
future mesh clients share one implementation.

Scoping is done by filtering resolved records to the mesh interface's
IP subnet. zeroconf does not accept an interface argument directly, so
the module reads the bat0 IP with SIOCGIFADDR and rejects advertisements
from outside the same /24.
"""

from __future__ import annotations

import asyncio
import fcntl
import ipaddress
import socket
import struct
from dataclasses import dataclass

from ados.core.logging import get_logger

log = get_logger("ground_station.mdns_announce")


@dataclass
class ResolvedService:
    host: str
    ip: str
    port: int


def iface_ip(iface: str) -> str | None:
    """Return IPv4 address of `iface` or None if unassigned."""
    try:
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        try:
            packed = fcntl.ioctl(
                s.fileno(),
                0x8915,  # SIOCGIFADDR
                struct.pack("256s", iface.encode()[:15]),
            )
            return socket.inet_ntoa(packed[20:24])
        finally:
            s.close()
    except OSError:
        return None


def same_subnet(a: str, b: str, mask_prefix: int = 24) -> bool:
    """True if IPv4 `a` is in the same /N as `b`."""
    try:
        net = ipaddress.ip_network(f"{b}/{mask_prefix}", strict=False)
        return ipaddress.ip_address(a) in net
    except ValueError:
        return False


class ReceiverAnnouncer:
    """Publish `_ados-receiver._tcp` on a mesh interface.

    Usage:

        ann = ReceiverAnnouncer(service="_ados-receiver._tcp", port=5800, iface="bat0")
        ann.start()
        ...
        ann.stop()
    """

    def __init__(self, service: str, port: int, iface: str) -> None:
        self._service = service
        self._port = port
        self._iface = iface
        self._zc: object | None = None
        self._info: object | None = None

    def start(self) -> bool:
        try:
            from zeroconf import ServiceInfo, Zeroconf
        except ImportError:
            log.error("zeroconf_not_installed")
            return False

        ip = iface_ip(self._iface)
        if ip is None:
            log.warning("mdns_announce_no_iface_ip", iface=self._iface)
            return False

        hostname = socket.gethostname()
        info = ServiceInfo(
            type_=self._service.rstrip(".") + ".local.",
            name=f"{hostname}.{self._service.rstrip('.')}.local.",
            addresses=[socket.inet_aton(ip)],
            port=self._port,
            properties={},
            server=f"{hostname}.local.",
        )
        zc = Zeroconf()
        try:
            zc.register_service(info)
        except Exception as exc:
            log.error("mdns_register_failed", error=str(exc))
            zc.close()
            return False

        self._zc = zc
        self._info = info
        log.info(
            "mdns_registered",
            service=self._service,
            ip=ip,
            port=self._port,
            iface=self._iface,
        )
        return True

    def stop(self) -> None:
        zc = self._zc
        if zc is None:
            return
        try:
            if self._info is not None:
                zc.unregister_service(self._info)  # type: ignore[attr-defined]
        except Exception:
            pass
        try:
            zc.close()  # type: ignore[attr-defined]
        except Exception:
            pass
        self._zc = None
        self._info = None


async def resolve_receiver(
    service: str,
    iface: str,
    timeout: float = 3.0,
) -> ResolvedService | None:
    """Resolve a `_ados-receiver._tcp` advertiser on `iface` subnet.

    Returns the first match whose IP sits in the same /24 as `iface`.
    """
    try:
        from zeroconf import ServiceBrowser, Zeroconf
    except ImportError:
        log.error("zeroconf_not_installed")
        return None

    iface_addr = iface_ip(iface)
    service_full = service.rstrip(".") + ".local."
    loop = asyncio.get_event_loop()
    found: asyncio.Future[ResolvedService] = loop.create_future()

    class _Listener:
        def add_service(self, zc, stype, name):  # noqa: ANN001
            info = zc.get_service_info(stype, name, timeout=1500)
            if info is None or not info.addresses:
                return
            for addr_bytes in info.addresses:
                try:
                    ip = socket.inet_ntoa(addr_bytes)
                except OSError:
                    continue
                if iface_addr and not same_subnet(ip, iface_addr):
                    continue
                if not found.done():
                    host = name.rstrip(".").split(".")[0]
                    found.set_result(ResolvedService(host=host, ip=ip, port=info.port))
                return

        def remove_service(self, *args, **kwargs):  # noqa: ANN001
            pass

        def update_service(self, *args, **kwargs):  # noqa: ANN001
            pass

    zc = Zeroconf()
    try:
        ServiceBrowser(zc, service_full, _Listener())
        try:
            return await asyncio.wait_for(found, timeout=timeout)
        except asyncio.TimeoutError:
            return None
    finally:
        try:
            zc.close()
        except Exception:
            pass
