"""mDNS announce and resolve helpers scoped to a single interface.

Two flavors of advertisement live here:

* `ReceiverAnnouncer` publishes `_ados-receiver._tcp` on a mesh
  interface (e.g. `bat0`) so relays can find receivers without shared
  infra.
* `APAnnouncer` publishes `_ados._tcp` on the wlan0 AP so phones,
  tablets, and laptops joining the ADOS-GS-* SSID can resolve the
  agent's REST/WS port without hardcoding `192.168.4.1`.

Scoping is done by filtering resolved records to the target interface's
IP subnet. zeroconf does not accept an interface argument directly, so
the module reads the interface IP with SIOCGIFADDR and rejects
advertisements from outside the same /24.
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
        except TimeoutError:
            return None
    finally:
        try:
            zc.close()
        except Exception:
            pass


# Default IP assigned to the AP interface. Matches the value rendered
# into hostapd + dnsmasq configs by hostapd_manager.
_AP_DEFAULT_IP = "192.168.4.1"


class APAnnouncer:
    """Publish `_ados._tcp` on the wlan0 AP interface.

    Carries TXT records the Android client uses to pick the right
    endpoint:

      * profile=ground_station
      * version=<agent version>
      * device_id=<short id>
      * path=/api/v1/ground-station

    The announcement only goes out when wlan0 holds the expected AP IP
    (`192.168.4.1` by default). If the AP is down, `start()` returns
    False and the caller is expected to retry later. `stop()` is
    idempotent and safe to call when registration never succeeded.

    Usage:

        ann = APAnnouncer(port=8080, device_id="abc123", version="1.2.3")
        ann.start()
        ...
        ann.stop()
    """

    def __init__(
        self,
        port: int,
        device_id: str,
        version: str,
        iface: str = "wlan0",
        expected_ip: str = _AP_DEFAULT_IP,
        service: str = "_ados._tcp",
        path: str = "/api/v1/ground-station",
    ) -> None:
        self._service = service
        self._port = port
        self._iface = iface
        self._expected_ip = expected_ip
        self._device_id = device_id
        self._version = version
        self._path = path
        self._zc: object | None = None
        self._info: object | None = None

    def is_ap_up(self) -> bool:
        """True when wlan0 holds the expected AP address."""
        ip = iface_ip(self._iface)
        return ip == self._expected_ip

    def start(self) -> bool:
        try:
            from zeroconf import ServiceInfo, Zeroconf
        except ImportError:
            log.error("zeroconf_not_installed")
            return False

        ip = iface_ip(self._iface)
        if ip is None:
            log.warning("ap_announce_no_iface_ip", iface=self._iface)
            return False
        if ip != self._expected_ip:
            log.warning(
                "ap_announce_unexpected_ip",
                iface=self._iface,
                ip=ip,
                expected=self._expected_ip,
            )
            return False

        hostname = socket.gethostname()
        properties = {
            b"profile": b"ground_station",
            b"version": self._version.encode("ascii", errors="replace"),
            b"device_id": self._device_id.encode("ascii", errors="replace"),
            b"path": self._path.encode("ascii", errors="replace"),
        }
        info = ServiceInfo(
            type_=self._service.rstrip(".") + ".local.",
            name=f"{hostname}.{self._service.rstrip('.')}.local.",
            addresses=[socket.inet_aton(ip)],
            port=self._port,
            properties=properties,
            server=f"{hostname}.local.",
        )
        zc = Zeroconf()
        try:
            zc.register_service(info)
        except Exception as exc:
            log.error("ap_announce_register_failed", error=str(exc))
            try:
                zc.close()
            except Exception:
                pass
            return False

        self._zc = zc
        self._info = info
        log.info(
            "ap_announce_registered",
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
        log.info("ap_announce_unregistered", iface=self._iface)
