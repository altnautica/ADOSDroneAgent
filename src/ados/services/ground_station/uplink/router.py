"""Uplink router orchestrator.

The ground station can reach the cloud over several uplinks: wired
Ethernet (`eth0`), WiFi client (`wlan0_client` when the onboard radio
is in STA mode, which is mutually exclusive with AP mode), cellular
(`wwan0` on the SIM7600G-H), and USB tether (`usb0` when a laptop
shares its connection over USB gadget).

This orchestrator picks exactly one of those as the default route based
on a configured priority list, and fails over automatically when the
active uplink stops reaching `convex.altnautica.com`.

Three concerns wired into one asyncio loop:

1. `UplinkRouter`. Priority chain plus hysteresis plus routing table.
2. The health checker. TCP connect to the cloud relay, bound to the
   current uplink's interface. Implementation lives in `health.py`.
3. `DataCapTracker`. Reads modem byte counters, persists cumulative
   usage, emits threshold events. Lives in `data_cap.py`.

Failover policy (priority load/save, hysteresis thresholds, route
replace, target selection) lives in `failover.py`. The systemd service
entry point and data-cap throttle consumer live in `service.py`.
"""

from __future__ import annotations

import asyncio
import subprocess
import time
from pathlib import Path
from typing import Any, Optional

import structlog

from ados.core.paths import GS_UPLINK_JSON

from . import failover as _failover
from . import health as _health
from .data_cap import DataCapTracker
from .events import UplinkEvent, UplinkEventBus
from .protocols import _StubManager

__all__ = ["UplinkRouter"]

log = structlog.get_logger(__name__)


class UplinkRouter:
    """Priority-based uplink failover with hysteresis and health probing."""

    def __init__(
        self,
        modem_manager: Optional[Any] = None,
        wifi_client_manager: Optional[Any] = None,
        ethernet_manager: Optional[Any] = None,
        usb_tether_check: Optional[Any] = None,
        priority: Optional[list[str]] = None,
        priority_config_path: Path = GS_UPLINK_JSON,
    ) -> None:
        self._modem = modem_manager
        self._wifi = wifi_client_manager or _StubManager("wlan0_client")
        self._eth = ethernet_manager or _StubManager("eth0")
        self._usb_check = usb_tether_check

        self._priority_config_path = priority_config_path
        self._priority = priority or _failover.load_priority(
            self._priority_config_path
        )

        self.active_uplink: Optional[str] = None
        self.internet_reachable: bool = False
        self._fail_streak: int = 0
        self._success_streak: int = 0
        self._last_switch_at: float = 0.0

        self.bus = UplinkEventBus()
        self._lock = asyncio.Lock()
        self._stop = asyncio.Event()
        self._health_task: Optional[asyncio.Task] = None
        self._manager_event_tasks: list[asyncio.Task] = []

        self.data_cap = (
            DataCapTracker(self._modem, self.bus) if self._modem is not None else None
        )

    # ------------------------------------------------------------------
    # Manager event bridge
    # ------------------------------------------------------------------
    def bind_manager_events(self) -> None:
        """Subscribe to WiFi and Ethernet manager buses.

        On every manager event, trigger an async re-evaluation of the
        uplink table. This avoids waiting for the 15s health-ping cycle
        when a link flaps. Safe to call once after managers are wired.
        """
        wifi_bus = getattr(self._wifi, "bus", None)
        eth_bus = getattr(self._eth, "bus", None)

        if wifi_bus is not None and hasattr(wifi_bus, "subscribe"):
            self._manager_event_tasks.append(
                asyncio.create_task(self._consume_manager_bus("wifi", wifi_bus))
            )
        if eth_bus is not None and hasattr(eth_bus, "subscribe"):
            self._manager_event_tasks.append(
                asyncio.create_task(self._consume_manager_bus("eth", eth_bus))
            )

    async def _consume_manager_bus(self, source: str, mgr_bus: Any) -> None:
        try:
            async for _evt in mgr_bus.subscribe():
                if self._stop.is_set():
                    return
                try:
                    await self._tick()
                except Exception as exc:
                    log.debug(
                        "uplink.manager_event_tick_failed",
                        source=source,
                        error=str(exc),
                    )
        except asyncio.CancelledError:
            return
        except Exception as exc:
            log.warning(
                "uplink.manager_bus_consumer_failed",
                source=source,
                error=str(exc),
            )

    # ------------------------------------------------------------------
    # Priority list
    # ------------------------------------------------------------------
    def get_priority(self) -> list[str]:
        return list(self._priority)

    def set_priority(self, priority_list: list[str]) -> None:
        _failover.validate_priority(priority_list)
        self._priority = list(priority_list)
        _failover.save_priority(self._priority_config_path, self._priority)
        log.info("uplink.priority_updated", priority=self._priority)

    # ------------------------------------------------------------------
    # Uplink probing
    # ------------------------------------------------------------------
    async def _manager_for(self, name: str) -> Optional[Any]:
        if name == "wwan0":
            return self._modem
        if name == "wlan0_client":
            return self._wifi
        if name == "eth0":
            return self._eth
        return None

    async def _is_usb_tether_up(self) -> bool:
        # usb0 is brought up by the USB-gadget setup webapp flow. We
        # detect it by presence + carrier, without a dedicated manager.
        if self._usb_check is not None:
            try:
                return bool(await self._usb_check())
            except Exception as exc:
                log.debug("uplink.usb_check_failed", error=str(exc))
                return False
        try:
            carrier = Path("/sys/class/net/usb0/carrier")
            if not carrier.exists():
                return False
            return carrier.read_text().strip() == "1"
        except OSError:
            return False

    async def _uplink_up(self, name: str) -> bool:
        if name == "usb0":
            return await self._is_usb_tether_up()
        mgr = await self._manager_for(name)
        if mgr is None:
            return False
        try:
            return await mgr.is_up()
        except Exception as exc:
            log.debug("uplink.is_up_failed", uplink=name, error=str(exc))
            return False

    async def _uplink_iface(self, name: str) -> Optional[str]:
        if name == "usb0":
            return "usb0"
        mgr = await self._manager_for(name)
        if mgr is None:
            return None
        getter = getattr(mgr, "get_iface", None)
        if callable(getter):
            try:
                return getter()
            except Exception:
                return None
        return name

    async def _uplink_gateway(self, name: str) -> Optional[str]:
        if name == "usb0":
            return self._read_iface_gateway("usb0")
        mgr = await self._manager_for(name)
        if mgr is None:
            return None
        getter = getattr(mgr, "get_gateway", None)
        if getter is None:
            return None
        try:
            return await getter()
        except Exception as exc:
            log.debug("uplink.get_gateway_failed", uplink=name, error=str(exc))
            return None

    def _read_iface_gateway(self, iface: str) -> Optional[str]:
        try:
            result = subprocess.run(
                ["ip", "route", "show", "dev", iface],
                check=False,
                capture_output=True,
                timeout=3,
            )
            text = result.stdout.decode(errors="replace")
            for line in text.splitlines():
                parts = line.split()
                if "default" in parts and "via" in parts:
                    idx = parts.index("via")
                    if idx + 1 < len(parts):
                        return parts[idx + 1]
        except (OSError, subprocess.SubprocessError):
            return None
        return None

    async def _viable_uplinks(self) -> list[str]:
        viable: list[str] = []
        for name in self._priority:
            if await self._uplink_up(name):
                viable.append(name)
        return viable

    # ------------------------------------------------------------------
    # Routing table + health probe shims
    # ------------------------------------------------------------------
    def _apply_default_route(self, iface: str, gateway: Optional[str]) -> bool:
        return _failover.apply_default_route(iface, gateway)

    async def _probe_host(self, iface: Optional[str]) -> bool:
        return await _health.probe_host(iface)

    # ------------------------------------------------------------------
    # Control loop
    # ------------------------------------------------------------------
    def _now_ms(self) -> int:
        return int(time.time() * 1000)

    def _data_cap_state(self) -> Optional[str]:
        if self.data_cap is None:
            return None
        return self.data_cap.get_usage()["state"]

    async def _publish_health_change(
        self,
        active: Optional[str],
        available: list[str],
        reachable: bool,
    ) -> None:
        await self.bus.publish(
            UplinkEvent(
                kind="health_changed",
                active_uplink=active,
                available=available,
                internet_reachable=reachable,
                data_cap_state=self._data_cap_state(),
                timestamp_ms=self._now_ms(),
            )
        )

    async def _switch_to(
        self, uplink: Optional[str], available: list[str]
    ) -> None:
        previous = self.active_uplink
        self.active_uplink = uplink
        self._fail_streak = 0
        self._success_streak = 0
        self._last_switch_at = time.monotonic()

        if uplink is not None:
            iface = await self._uplink_iface(uplink) or uplink
            gateway = await self._uplink_gateway(uplink)
            self._apply_default_route(iface, gateway)

        log.info(
            "uplink.switched",
            previous=previous,
            current=uplink,
            available=available,
        )
        await self.bus.publish(
            UplinkEvent(
                kind="uplink_changed",
                active_uplink=uplink,
                available=available,
                internet_reachable=self.internet_reachable,
                data_cap_state=self._data_cap_state(),
                timestamp_ms=self._now_ms(),
            )
        )

    async def _tick(self) -> None:
        async with self._lock:
            available = await self._viable_uplinks()

            # No viable uplink at all. Clear state if we had one.
            if not available:
                if self.active_uplink is not None:
                    await self._switch_to(None, [])
                if self.internet_reachable:
                    self.internet_reachable = False
                    await self._publish_health_change(None, [], False)
                return

            # First-time pick. Use highest-priority viable uplink.
            if self.active_uplink is None:
                await self._switch_to(available[0], available)

            # Probe the current uplink.
            iface = await self._uplink_iface(self.active_uplink or "")
            ok = await self._probe_host(iface)

            cooldown_ok = (
                time.monotonic() - self._last_switch_at
            ) >= _failover.SWITCH_COOLDOWN_SECONDS

            if ok:
                await self._handle_probe_success(available, cooldown_ok)
                return

            await self._handle_probe_failure(available, cooldown_ok)

    async def _handle_probe_success(
        self, available: list[str], cooldown_ok: bool
    ) -> None:
        self._fail_streak = 0
        if not self.internet_reachable:
            self.internet_reachable = True
            log.info("uplink.health_recovered", uplink=self.active_uplink)
            await self._publish_health_change(
                self.active_uplink, available, True
            )

        if not (cooldown_ok and self.active_uplink is not None):
            return

        higher = _failover.select_higher_priority(
            self._priority, available, self.active_uplink
        )
        if not higher:
            self._success_streak = 0
            return

        self._success_streak += 1
        if self._success_streak < _failover.SUCCESS_UP_THRESHOLD:
            return

        # Probe the higher-priority uplink before switching up so we do
        # not drop off a working link for a dead one.
        candidate = higher[0]
        cand_iface = await self._uplink_iface(candidate) or candidate
        if await self._probe_host(cand_iface):
            await self._switch_to(candidate, available)

    async def _handle_probe_failure(
        self, available: list[str], cooldown_ok: bool
    ) -> None:
        self._success_streak = 0
        self._fail_streak += 1
        if self._fail_streak < _failover.FAIL_DOWN_THRESHOLD:
            return
        if not cooldown_ok:
            return

        next_uplink = _failover.select_failover_target(
            self._priority, available, self.active_uplink
        )
        if next_uplink is None:
            # Only the current (failing) uplink is available.
            if self.internet_reachable:
                self.internet_reachable = False
                await self._publish_health_change(
                    self.active_uplink, available, False
                )
            return

        log.warning(
            "uplink.failover",
            from_uplink=self.active_uplink,
            to_uplink=next_uplink,
            fail_streak=self._fail_streak,
        )
        self.internet_reachable = False
        await self._switch_to(next_uplink, available)

    async def _run_health_loop(self) -> None:
        log.info(
            "uplink.health_loop_start",
            priority=self._priority,
            host=_health.HEALTH_HOST,
        )
        while not self._stop.is_set():
            try:
                await self._tick()
            except Exception as exc:
                log.warning("uplink.tick_error", error=str(exc))
            try:
                await asyncio.wait_for(
                    self._stop.wait(),
                    timeout=_health.HEALTH_INTERVAL_SECONDS,
                )
                break
            except asyncio.TimeoutError:
                pass
        log.info("uplink.health_loop_stop")

    # ------------------------------------------------------------------
    # Lifecycle
    # ------------------------------------------------------------------
    async def start(self) -> None:
        if self._health_task is not None:
            return
        self._stop.clear()
        if self.data_cap is not None:
            await self.data_cap.start()
        self._health_task = asyncio.create_task(self._run_health_loop())
        log.info("uplink.router_started")

    async def stop(self) -> None:
        self._stop.set()
        if self._health_task is not None:
            self._health_task.cancel()
            try:
                await self._health_task
            except (asyncio.CancelledError, Exception):
                pass
            self._health_task = None
        for task in self._manager_event_tasks:
            task.cancel()
        for task in self._manager_event_tasks:
            try:
                await task
            except (asyncio.CancelledError, Exception):
                pass
        self._manager_event_tasks.clear()
        if self.data_cap is not None:
            await self.data_cap.stop()
        await self.bus.close()
        log.info("uplink.router_stopped")

    def get_state(self) -> dict:
        usage: Optional[dict] = None
        if self.data_cap is not None:
            usage = self.data_cap.get_usage()
        return {
            "active_uplink": self.active_uplink,
            "internet_reachable": self.internet_reachable,
            "priority": list(self._priority),
            "fail_streak": self._fail_streak,
            "success_streak": self._success_streak,
            "last_switch_monotonic": self._last_switch_at,
            "data_usage": usage,
        }
