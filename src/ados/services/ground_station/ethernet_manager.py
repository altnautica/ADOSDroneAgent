"""Ethernet (eth0) uplink manager for the ground-station profile (MSN-027).

Phase 3 uplink matrix. Much simpler than the WiFi client: most of the
work is passive link detection, because NetworkManager already brings
eth0 up on cable-plug via its default wired profile. This module:

1. Polls `/sys/class/net/eth0/carrier` every 5 s.
2. Emits EthernetEvent{kind, link_up, ip, timestamp_ms} on state change.
3. Exposes status + enable/disable for the uplink router.
4. Configure-static is a Phase 3 stub; DHCP is the default path.

Normally this manager runs embedded inside the uplink_router process.
A standalone `main()` is provided for bench testing and for the case
where Cellos Wave C decides to ship a dedicated systemd unit.
"""

from __future__ import annotations

import asyncio
import contextlib
import re
import signal
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import AsyncIterator, Literal

from ados.core.logging import get_logger

log = get_logger("ground_station.ethernet")

_ETH_IFACE = "eth0"
_CARRIER_PATH = Path(f"/sys/class/net/{_ETH_IFACE}/carrier")
_SPEED_PATH = Path(f"/sys/class/net/{_ETH_IFACE}/speed")


# -------------------- events --------------------


@dataclass(frozen=True)
class EthernetEvent:
    """Ethernet link state transition."""

    kind: Literal["link_up", "link_down"]
    link_up: bool
    ip: str | None
    timestamp_ms: int


class EthernetEventBus:
    """Asyncio fanout bus for EthernetEvent."""

    _SENTINEL: object = object()

    def __init__(self, queue_maxsize: int = 16) -> None:
        self._subscribers: list[asyncio.Queue] = []
        self._queue_maxsize = queue_maxsize
        self._closed = False
        self._lock = asyncio.Lock()

    async def publish(self, event: EthernetEvent) -> None:
        if self._closed:
            return
        async with self._lock:
            targets = list(self._subscribers)
        for q in targets:
            try:
                q.put_nowait(event)
            except asyncio.QueueFull:
                pass

    async def subscribe(self) -> AsyncIterator[EthernetEvent]:
        queue: asyncio.Queue = asyncio.Queue(maxsize=self._queue_maxsize)
        async with self._lock:
            if self._closed:
                return
            self._subscribers.append(queue)
        try:
            while True:
                item = await queue.get()
                if item is self._SENTINEL:
                    return
                assert isinstance(item, EthernetEvent)
                yield item
        finally:
            async with self._lock:
                if queue in self._subscribers:
                    self._subscribers.remove(queue)

    async def close(self) -> None:
        async with self._lock:
            self._closed = True
            targets = list(self._subscribers)
        for q in targets:
            with contextlib.suppress(asyncio.QueueFull):
                q.put_nowait(self._SENTINEL)


# -------------------- helpers --------------------


def _now_ms() -> int:
    return int(time.time() * 1000)


async def _run(cmd: list[str], timeout: float = 10.0) -> tuple[int, str, str]:
    try:
        proc = await asyncio.create_subprocess_exec(
            *cmd,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        try:
            out, err = await asyncio.wait_for(proc.communicate(), timeout=timeout)
        except asyncio.TimeoutError:
            proc.kill()
            await proc.wait()
            return 124, "", "timeout"
        return (
            proc.returncode or 0,
            out.decode(errors="replace"),
            err.decode(errors="replace"),
        )
    except (OSError, asyncio.CancelledError) as exc:
        return 1, "", str(exc)


def _read_carrier() -> bool:
    if not _CARRIER_PATH.exists():
        return False
    try:
        return _CARRIER_PATH.read_text(encoding="utf-8").strip() == "1"
    except OSError:
        return False


def _read_speed() -> int | None:
    if not _SPEED_PATH.exists():
        return None
    try:
        val = _SPEED_PATH.read_text(encoding="utf-8").strip()
        return int(val) if val and val != "-1" else None
    except (OSError, ValueError):
        return None


# -------------------- manager --------------------


class EthernetManager:
    """eth0 link manager."""

    def __init__(self, interface: str = _ETH_IFACE) -> None:
        self._interface = interface
        self._bus = EthernetEventBus()
        self._poll_task: asyncio.Task | None = None
        self._last_link: bool | None = None

    @property
    def bus(self) -> EthernetEventBus:
        return self._bus

    # -------------------- public API --------------------

    async def status(self) -> dict:
        """Return link + IP + gateway + DHCP lease info."""
        link = _read_carrier()
        speed = _read_speed() if link else None

        ip_addr = None
        gateway = None
        lease_remaining = None

        rc, out, _ = await _run(
            ["ip", "-4", "addr", "show", self._interface],
            timeout=5,
        )
        if rc == 0:
            m = re.search(r"inet\s+(\d+\.\d+\.\d+\.\d+)", out)
            if m:
                ip_addr = m.group(1)
            lease_m = re.search(r"valid_lft\s+(\d+)sec", out)
            if lease_m:
                try:
                    lease_remaining = int(lease_m.group(1))
                except ValueError:
                    lease_remaining = None

        rc2, out2, _ = await _run(
            ["ip", "-4", "route", "show", "default", "dev", self._interface],
            timeout=5,
        )
        if rc2 == 0:
            m = re.search(r"default\s+via\s+(\d+\.\d+\.\d+\.\d+)", out2)
            if m:
                gateway = m.group(1)

        return {
            "link": link,
            "speed_mbps": speed,
            "ip": ip_addr,
            "gateway": gateway,
            "dhcp_lease_remaining_s": lease_remaining,
        }

    async def enable(self) -> dict:
        """Ensure eth0 is up (idempotent)."""
        rc, out, err = await _run(
            ["ip", "link", "set", self._interface, "up"],
            timeout=5,
        )
        return {"ok": rc == 0, "error": err.strip() if rc != 0 else None}

    async def disable(self) -> dict:
        """Bring eth0 down."""
        rc, out, err = await _run(
            ["ip", "link", "set", self._interface, "down"],
            timeout=5,
        )
        return {"ok": rc == 0, "error": err.strip() if rc != 0 else None}

    async def configure_static(
        self,
        ip: str,
        gateway: str,
        dns: list[str],
    ) -> dict:
        """Phase 3 stub. Returns a deferred marker."""
        log.info(
            "configure_static_stub",
            ip=ip,
            gateway=gateway,
            dns=dns,
            note="DHCP only for Phase 3",
        )
        return {
            "ok": False,
            "error": "not_implemented_phase_3",
            "hint": "DHCP only for Phase 3",
        }

    # -------------------- background poll --------------------

    async def _poll_loop(self, interval_s: float = 5.0) -> None:
        while True:
            try:
                link = _read_carrier()
                if self._last_link is None:
                    self._last_link = link
                elif link != self._last_link:
                    st = await self.status()
                    await self._bus.publish(EthernetEvent(
                        kind="link_up" if link else "link_down",
                        link_up=link,
                        ip=st.get("ip"),
                        timestamp_ms=_now_ms(),
                    ))
                    self._last_link = link
                    log.info("ethernet_link_transition", link_up=link, ip=st.get("ip"))
            except Exception as exc:  # noqa: BLE001
                log.debug("ethernet_poll_error", error=str(exc))
            await asyncio.sleep(interval_s)

    def start_polling(self) -> None:
        if self._poll_task is None or self._poll_task.done():
            self._poll_task = asyncio.create_task(self._poll_loop())

    def stop_polling(self) -> None:
        if self._poll_task and not self._poll_task.done():
            self._poll_task.cancel()


# -------------------- singleton --------------------


_INSTANCE: EthernetManager | None = None


def get_ethernet_manager() -> EthernetManager:
    global _INSTANCE
    if _INSTANCE is None:
        _INSTANCE = EthernetManager()
    return _INSTANCE


# -------------------- optional standalone entry --------------------


async def main() -> None:
    """Optional standalone entry. Normal deployment embeds this manager
    inside the uplink_router process; this entry is for bench testing or
    if Cellos Wave C opts for a dedicated systemd unit."""
    from ados.core.config import load_config
    from ados.core.logging import configure_logging
    import structlog

    cfg = load_config()
    configure_logging(cfg.logging.level)
    slog = structlog.get_logger()
    slog.info("ethernet_service_starting")

    mgr = get_ethernet_manager()
    mgr.start_polling()

    shutdown = asyncio.Event()
    loop = asyncio.get_event_loop()
    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, shutdown.set)

    await shutdown.wait()
    mgr.stop_polling()
    await mgr.bus.close()
    slog.info("ethernet_service_stopped")


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
    sys.exit(0)
