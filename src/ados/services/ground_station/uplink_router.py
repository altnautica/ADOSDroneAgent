"""Uplink router + health monitor + data-cap tracker.

The ground station can reach the cloud over several uplinks: wired
Ethernet (`eth0`), WiFi client (`wlan0_client` when the onboard radio
is in STA mode, which is mutually exclusive with AP mode), cellular
(`wwan0` on the SIM7600G-H), and USB tether (`usb0` when a laptop
shares its connection over USB gadget).

This module picks exactly one of those as the default route based on a
configured priority list, and fails over automatically when the active
uplink stops reaching `convex.altnautica.com`. It also tracks cellular
data usage so the operator does not silently burn a SIM cap.

Three concerns, one asyncio loop:

1. `UplinkRouter`. Priority chain plus hysteresis plus routing table.
2. `_HealthChecker`. HEAD to the cloud relay, bound to the current
   uplink's interface so the probe actually tests the path.
3. `DataCapTracker`. Reads modem byte counters, persists cumulative
   usage, emits threshold events at 80 / 95 / 100 percent.

Events fan out on `UplinkEventBus`, which mirrors `ButtonEventBus`
from `src/ados/services/ui/events.py` and `PicEventBus` from
`pic_arbiter.py`. Bounded per-subscriber queues, drop-on-full.

Wiring: this file ships with stubbable manager references and is
later wired against the concrete `WifiClientManager`,
`EthernetManager`, and the cellular dependency on
`GroundStationModemManager`. The router asks each manager for
`is_up()`, `get_gateway()`, and (for modem) byte counters, so
adding a new uplink later is a matter of dropping in another
manager with the same three methods.
"""

from __future__ import annotations

import asyncio
import json
import os
import signal
import socket
import subprocess
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, AsyncIterator, Literal, Optional, Protocol

import structlog

log = structlog.get_logger(__name__)

__all__ = [
    "UplinkEvent",
    "UplinkEventBus",
    "UplinkManagerProto",
    "UplinkRouter",
    "DataCapTracker",
    "get_uplink_router",
    "main",
]


UplinkEventKind = Literal["uplink_changed", "health_changed", "data_cap_threshold"]
DataCapState = Literal["ok", "warn_80", "throttle_95", "blocked_100"]

_PRIORITY_CONFIG_PATH = Path("/etc/ados/ground-station-uplink.json")
_USAGE_STATE_PATH = Path("/var/lib/ados/modem-usage.json")

# Default priority chain. `wlan0_ap` is the LAN-side SSID served to
# phones and laptops, not an uplink, so it is absent here.
_DEFAULT_PRIORITY: list[str] = ["eth0", "wlan0_client", "wwan0", "usb0"]

# Per-uplink route metric. Lower number wins in the kernel routing
# table, so we keep the gap large to survive manual `ip route` probes.
_PRIORITY_METRIC = {
    "eth0": 100,
    "wlan0_client": 200,
    "wwan0": 300,
    "usb0": 400,
}

# Hysteresis knobs. Three consecutive fails flip us down to the next
# viable uplink. Three consecutive successes on a higher-priority
# uplink flip us back up. A 30 second cooldown between switches
# prevents thrash when two uplinks are both flaky.
_FAIL_DOWN_THRESHOLD = 3
_SUCCESS_UP_THRESHOLD = 3
_SWITCH_COOLDOWN_SECONDS = 30.0

_HEALTH_INTERVAL_SECONDS = 15.0
_HEALTH_TIMEOUT_SECONDS = 5.0
_HEALTH_HOST = "convex.altnautica.com"
_HEALTH_PORT = 443
_HEALTH_PATH = "/"

# Data-cap polling.
_DATA_CAP_INTERVAL_SECONDS = 60.0
_DEFAULT_CAP_GB = 5.0


@dataclass(frozen=True)
class UplinkEvent:
    """A routing, health, or data-cap state change."""

    kind: UplinkEventKind
    active_uplink: Optional[str]
    available: list[str]
    internet_reachable: bool
    data_cap_state: Optional[DataCapState]
    timestamp_ms: int


class UplinkEventBus:
    """Fanout bus for `UplinkEvent`. Bounded queues, drop-on-full."""

    _SENTINEL: object = object()

    def __init__(self, queue_maxsize: int = 64) -> None:
        self._subscribers: list[asyncio.Queue] = []
        self._queue_maxsize = queue_maxsize
        self._closed = False
        self._lock = asyncio.Lock()

    async def publish(self, event: UplinkEvent) -> None:
        if self._closed:
            return
        async with self._lock:
            targets = list(self._subscribers)
        for q in targets:
            try:
                q.put_nowait(event)
            except asyncio.QueueFull:
                pass

    async def subscribe(self) -> AsyncIterator[UplinkEvent]:
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
                assert isinstance(item, UplinkEvent)
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
            try:
                q.put_nowait(self._SENTINEL)
            except asyncio.QueueFull:
                try:
                    q.get_nowait()
                    q.put_nowait(self._SENTINEL)
                except Exception:
                    pass


class UplinkManagerProto(Protocol):
    """Structural type every uplink manager satisfies.

    The router only uses `is_up`, `get_iface`, and `get_gateway`. The
    modem manager also exposes `data_usage` for the cap tracker.
    """

    async def is_up(self) -> bool: ...
    def get_iface(self) -> str: ...
    async def get_gateway(self) -> Optional[str]: ...


class _StubManager:
    """Inert placeholder so `UplinkRouter` can run before the real
    Ethernet / WiFi client managers are wired in.

    `is_up()` returns False so the stubbed uplink never passes
    `_viable_uplinks()`. The real manager replaces the stub through
    the router constructor.
    """

    def __init__(self, iface: str) -> None:
        self._iface = iface

    async def is_up(self) -> bool:
        return False

    def get_iface(self) -> str:
        return self._iface

    async def get_gateway(self) -> Optional[str]:
        return None


class _ModemAdapter:
    """Adapts `GroundStationModemManager` to the uplink protocol.

    The concrete modem manager exposes `status()` and `data_usage()`
    but not `is_up / get_iface / get_gateway`. This adapter bridges
    the gap without modifying `modem_manager.py`. It also forwards
    `data_usage()` so `DataCapTracker` keeps working.
    """

    def __init__(self, modem: Any, iface: str = "wwan0") -> None:
        self._modem = modem
        self._iface = iface

    async def is_up(self) -> bool:
        try:
            st = await self._modem.status()
        except Exception as exc:
            log.debug("uplink.modem_status_failed", error=str(exc))
            return False
        state = str(st.get("state", "")).lower()
        if state in ("connected", "registered", "online"):
            return True
        return bool(st.get("ip")) or bool(st.get("connected"))

    def get_iface(self) -> str:
        getter = getattr(self._modem, "_current_iface", None)
        if callable(getter):
            try:
                return getter()
            except Exception:
                pass
        return self._iface

    async def get_gateway(self) -> Optional[str]:
        iface = self.get_iface()
        try:
            proc = await asyncio.create_subprocess_exec(
                "ip", "-4", "route", "show", "default", "dev", iface,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.DEVNULL,
            )
            stdout, _ = await proc.communicate()
            text = stdout.decode(errors="replace")
            for line in text.splitlines():
                parts = line.split()
                if "default" in parts and "via" in parts:
                    i = parts.index("via")
                    if i + 1 < len(parts):
                        return parts[i + 1]
        except (OSError, asyncio.CancelledError):
            return None
        return None

    async def data_usage(self) -> dict:
        return await self._modem.data_usage()


class _WifiClientAdapter:
    """Adapts `WifiClientManager` to the uplink protocol via status()."""

    def __init__(self, wifi: Any, iface: str = "wlan0_client") -> None:
        self._wifi = wifi
        self._iface = iface

    async def is_up(self) -> bool:
        try:
            st = await self._wifi.status()
        except Exception as exc:
            log.debug("uplink.wifi_status_failed", error=str(exc))
            return False
        return bool(st.get("connected")) and bool(st.get("ip"))

    def get_iface(self) -> str:
        inner = getattr(self._wifi, "_interface", None)
        if isinstance(inner, str) and inner:
            return inner
        return self._iface

    async def get_gateway(self) -> Optional[str]:
        try:
            st = await self._wifi.status()
        except Exception:
            return None
        gw = st.get("gateway")
        return gw if isinstance(gw, str) and gw else None


class _EthernetAdapter:
    """Adapts `EthernetManager` to the uplink protocol via status()."""

    def __init__(self, eth: Any, iface: str = "eth0") -> None:
        self._eth = eth
        self._iface = iface

    async def is_up(self) -> bool:
        try:
            st = await self._eth.status()
        except Exception as exc:
            log.debug("uplink.eth_status_failed", error=str(exc))
            return False
        return bool(st.get("link")) and bool(st.get("ip"))

    def get_iface(self) -> str:
        inner = getattr(self._eth, "_interface", None)
        if isinstance(inner, str) and inner:
            return inner
        return self._iface

    async def get_gateway(self) -> Optional[str]:
        try:
            st = await self._eth.status()
        except Exception:
            return None
        gw = st.get("gateway")
        return gw if isinstance(gw, str) and gw else None


# ----------------------------------------------------------------------
# Data cap tracker
# ----------------------------------------------------------------------


@dataclass
class _UsageState:
    window_started_at: float = field(default_factory=time.time)
    cumulative_bytes: int = 0
    last_rx: int = 0
    last_tx: int = 0
    last_reset_month: str = ""

    def to_json(self) -> dict:
        return {
            "window_started_at": self.window_started_at,
            "cumulative_bytes": self.cumulative_bytes,
            "last_rx": self.last_rx,
            "last_tx": self.last_tx,
            "last_reset_month": self.last_reset_month,
        }

    @classmethod
    def from_json(cls, data: dict) -> "_UsageState":
        return cls(
            window_started_at=float(data.get("window_started_at", time.time())),
            cumulative_bytes=int(data.get("cumulative_bytes", 0)),
            last_rx=int(data.get("last_rx", 0)),
            last_tx=int(data.get("last_tx", 0)),
            last_reset_month=str(data.get("last_reset_month", "")),
        )


class DataCapTracker:
    """Tracks cellular data usage across month windows.

    Polls the modem manager at 60 second intervals, accumulates bytes,
    persists to `/var/lib/ados/modem-usage.json`, and emits threshold
    events when crossing 80, 95, and 100 percent of the configured cap.
    """

    def __init__(
        self,
        modem_manager: Any,
        bus: UplinkEventBus,
        cap_gb: float = _DEFAULT_CAP_GB,
        state_path: Path = _USAGE_STATE_PATH,
    ) -> None:
        self._modem = modem_manager
        self._bus = bus
        self._cap_bytes = int(cap_gb * 1024 * 1024 * 1024)
        self._state_path = state_path
        self._state = self._load_state()
        self._last_threshold: Optional[DataCapState] = None
        self._task: Optional[asyncio.Task] = None
        self._stop = asyncio.Event()

    def set_cap(self, gb: float) -> None:
        self._cap_bytes = int(gb * 1024 * 1024 * 1024)
        log.info("uplink.datacap_set", cap_gb=gb)

    def _load_state(self) -> _UsageState:
        try:
            if self._state_path.exists():
                raw = json.loads(self._state_path.read_text(encoding="utf-8"))
                return _UsageState.from_json(raw)
        except (OSError, ValueError) as exc:
            log.warning("uplink.datacap_load_failed", error=str(exc))
        return _UsageState(last_reset_month=time.strftime("%Y-%m"))

    def _save_state(self) -> None:
        try:
            self._state_path.parent.mkdir(parents=True, exist_ok=True)
            tmp = self._state_path.with_suffix(".json.tmp")
            tmp.write_text(
                json.dumps(self._state.to_json()),
                encoding="utf-8",
            )
            os.replace(tmp, self._state_path)
        except OSError as exc:
            log.warning("uplink.datacap_save_failed", error=str(exc))

    def _current_month(self) -> str:
        return time.strftime("%Y-%m")

    def _check_month_reset(self) -> bool:
        now_month = self._current_month()
        if self._state.last_reset_month != now_month:
            log.info(
                "uplink.datacap_month_reset",
                from_month=self._state.last_reset_month,
                to_month=now_month,
                bytes_used=self._state.cumulative_bytes,
            )
            self._state = _UsageState(
                window_started_at=time.time(),
                cumulative_bytes=0,
                last_rx=0,
                last_tx=0,
                last_reset_month=now_month,
            )
            self._last_threshold = None
            self._save_state()
            return True
        return False

    def _classify(self) -> DataCapState:
        if self._cap_bytes <= 0:
            return "ok"
        pct = (self._state.cumulative_bytes / self._cap_bytes) * 100.0
        if pct >= 100.0:
            return "blocked_100"
        if pct >= 95.0:
            return "throttle_95"
        if pct >= 80.0:
            return "warn_80"
        return "ok"

    def _now_ms(self) -> int:
        return int(time.time() * 1000)

    async def _poll_once(self) -> None:
        usage_fn = getattr(self._modem, "data_usage", None)
        if usage_fn is None:
            return
        try:
            usage = await usage_fn()
        except Exception as exc:
            log.debug("uplink.datacap_poll_failed", error=str(exc))
            return

        rx = int(usage.get("rx_bytes", 0))
        tx = int(usage.get("tx_bytes", 0))

        # Handle modem counter resets across reboots. If the new value
        # is smaller than the previous sample we assume a reset and
        # start counting from the new baseline.
        drx = max(0, rx - self._state.last_rx)
        dtx = max(0, tx - self._state.last_tx)
        if rx < self._state.last_rx or tx < self._state.last_tx:
            drx = 0
            dtx = 0

        self._state.last_rx = rx
        self._state.last_tx = tx
        self._state.cumulative_bytes += drx + dtx
        self._save_state()

        new_state = self._classify()
        if new_state != self._last_threshold:
            self._last_threshold = new_state
            log.info(
                "uplink.datacap_threshold",
                state=new_state,
                used_mb=self._state.cumulative_bytes // (1024 * 1024),
                cap_mb=self._cap_bytes // (1024 * 1024),
            )
            await self._bus.publish(
                UplinkEvent(
                    kind="data_cap_threshold",
                    active_uplink=None,
                    available=[],
                    internet_reachable=True,
                    data_cap_state=new_state,
                    timestamp_ms=self._now_ms(),
                )
            )

    def get_usage(self) -> dict:
        used_mb = self._state.cumulative_bytes // (1024 * 1024)
        cap_mb = self._cap_bytes // (1024 * 1024)
        pct = 0.0
        if self._cap_bytes > 0:
            pct = (self._state.cumulative_bytes / self._cap_bytes) * 100.0
        return {
            "data_used_mb": used_mb,
            "cap_mb": cap_mb,
            "percent": round(pct, 2),
            "state": self._classify(),
            "window_reset_at": self._state.window_started_at,
            "last_reset_month": self._state.last_reset_month,
        }

    async def start(self) -> None:
        if self._task is not None:
            return
        self._stop.clear()
        self._task = asyncio.create_task(self._run())

    async def stop(self) -> None:
        self._stop.set()
        if self._task is not None:
            self._task.cancel()
            try:
                await self._task
            except (asyncio.CancelledError, Exception):
                pass
            self._task = None

    async def _run(self) -> None:
        log.info(
            "uplink.datacap_started",
            cap_gb=self._cap_bytes / (1024 ** 3),
            state_path=str(self._state_path),
        )
        while not self._stop.is_set():
            self._check_month_reset()
            await self._poll_once()
            try:
                await asyncio.wait_for(
                    self._stop.wait(), timeout=_DATA_CAP_INTERVAL_SECONDS
                )
                break
            except asyncio.TimeoutError:
                pass
        log.info("uplink.datacap_stopped")


# ----------------------------------------------------------------------
# Uplink router
# ----------------------------------------------------------------------


class UplinkRouter:
    """Priority-based uplink failover with hysteresis and health probing."""

    def __init__(
        self,
        modem_manager: Optional[Any] = None,
        wifi_client_manager: Optional[Any] = None,
        ethernet_manager: Optional[Any] = None,
        usb_tether_check: Optional[Any] = None,
        priority: Optional[list[str]] = None,
        priority_config_path: Path = _PRIORITY_CONFIG_PATH,
    ) -> None:
        self._modem = modem_manager
        self._wifi = wifi_client_manager or _StubManager("wlan0_client")
        self._eth = ethernet_manager or _StubManager("eth0")
        self._usb_check = usb_tether_check

        self._priority_config_path = priority_config_path
        self._priority = priority or self._load_priority()

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
    # Priority list persistence
    # ------------------------------------------------------------------
    def _load_priority(self) -> list[str]:
        try:
            if self._priority_config_path.exists():
                raw = json.loads(
                    self._priority_config_path.read_text(encoding="utf-8")
                )
                order = raw.get("priority")
                if isinstance(order, list) and all(isinstance(x, str) for x in order):
                    if order:
                        return order
        except (OSError, ValueError) as exc:
            log.warning("uplink.priority_load_failed", error=str(exc))
        return list(_DEFAULT_PRIORITY)

    def _save_priority(self) -> None:
        try:
            self._priority_config_path.parent.mkdir(parents=True, exist_ok=True)
            tmp = self._priority_config_path.with_suffix(".json.tmp")
            tmp.write_text(
                json.dumps({"priority": self._priority}), encoding="utf-8"
            )
            os.replace(tmp, self._priority_config_path)
        except OSError as exc:
            log.warning("uplink.priority_save_failed", error=str(exc))

    def get_priority(self) -> list[str]:
        return list(self._priority)

    def set_priority(self, priority_list: list[str]) -> None:
        if not priority_list or not all(isinstance(x, str) for x in priority_list):
            raise ValueError("priority must be a non-empty list of strings")
        self._priority = list(priority_list)
        self._save_priority()
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
            # USB gadget gateway is the laptop side. Read from the
            # routing table rather than hardcoding.
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
    # Routing table
    # ------------------------------------------------------------------
    def _apply_default_route(self, iface: str, gateway: Optional[str]) -> bool:
        metric = _PRIORITY_METRIC.get(iface, 500)
        # Clear our own metric slot first so `replace` is idempotent.
        cmd: list[str]
        if gateway:
            cmd = [
                "ip", "route", "replace", "default",
                "via", gateway, "dev", iface,
                "metric", str(metric),
            ]
        else:
            cmd = [
                "ip", "route", "replace", "default",
                "dev", iface, "metric", str(metric),
            ]
        try:
            result = subprocess.run(
                cmd, check=False, capture_output=True, timeout=5
            )
            if result.returncode != 0:
                log.warning(
                    "uplink.route_replace_failed",
                    cmd=" ".join(cmd),
                    rc=result.returncode,
                    stderr=result.stderr.decode(errors="replace").strip(),
                )
                return False
            log.info(
                "uplink.route_applied",
                iface=iface,
                gateway=gateway,
                metric=metric,
            )
            return True
        except (OSError, subprocess.SubprocessError) as exc:
            log.warning("uplink.route_apply_exc", error=str(exc))
            return False

    # ------------------------------------------------------------------
    # Health probe
    # ------------------------------------------------------------------
    async def _probe_host(self, iface: Optional[str]) -> bool:
        """TCP connect + minimal HTTPS HEAD-style handshake.

        We do not attempt a real TLS handshake from stdlib to keep the
        dep footprint minimal. A successful TCP connect to port 443 is
        a strong proxy for reachability of the Cloudflare-fronted
        Convex endpoint. On failure we log and return False.
        """
        loop = asyncio.get_running_loop()
        try:
            sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            sock.setblocking(False)
            if iface is not None:
                try:
                    sock.setsockopt(
                        socket.SOL_SOCKET,
                        socket.SO_BINDTODEVICE,
                        iface.encode("ascii"),
                    )
                except (PermissionError, OSError) as exc:
                    # SO_BINDTODEVICE needs CAP_NET_RAW. If unavailable
                    # we fall back to a plain connect on the current
                    # default route, which still validates reachability.
                    log.debug(
                        "uplink.bind_iface_failed",
                        iface=iface,
                        error=str(exc),
                    )
            try:
                addr_info = await loop.getaddrinfo(
                    _HEALTH_HOST, _HEALTH_PORT, type=socket.SOCK_STREAM
                )
            except OSError as exc:
                log.debug("uplink.dns_failed", error=str(exc))
                sock.close()
                return False
            if not addr_info:
                sock.close()
                return False
            family, socktype, proto, _, sockaddr = addr_info[0]
            if family != socket.AF_INET:
                # Rebuild with matching family.
                sock.close()
                sock = socket.socket(family, socktype, proto)
                sock.setblocking(False)
            try:
                await asyncio.wait_for(
                    loop.sock_connect(sock, sockaddr),
                    timeout=_HEALTH_TIMEOUT_SECONDS,
                )
                return True
            except (asyncio.TimeoutError, OSError) as exc:
                log.debug(
                    "uplink.connect_failed", iface=iface, error=str(exc)
                )
                return False
            finally:
                try:
                    sock.close()
                except OSError:
                    pass
        except Exception as exc:
            log.debug("uplink.probe_exc", error=str(exc))
            return False

    # ------------------------------------------------------------------
    # Control loop
    # ------------------------------------------------------------------
    def _now_ms(self) -> int:
        return int(time.time() * 1000)

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
                data_cap_state=(
                    self.data_cap.get_usage()["state"]
                    if self.data_cap is not None
                    else None
                ),
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
                    await self.bus.publish(
                        UplinkEvent(
                            kind="health_changed",
                            active_uplink=None,
                            available=[],
                            internet_reachable=False,
                            data_cap_state=(
                                self.data_cap.get_usage()["state"]
                                if self.data_cap is not None
                                else None
                            ),
                            timestamp_ms=self._now_ms(),
                        )
                    )
                return

            # First-time pick. Use highest-priority viable uplink.
            if self.active_uplink is None:
                await self._switch_to(available[0], available)

            # Probe the current uplink.
            iface = await self._uplink_iface(self.active_uplink or "")
            ok = await self._probe_host(iface)

            cooldown_ok = (
                time.monotonic() - self._last_switch_at
            ) >= _SWITCH_COOLDOWN_SECONDS

            if ok:
                self._fail_streak = 0
                if not self.internet_reachable:
                    self.internet_reachable = True
                    log.info(
                        "uplink.health_recovered", uplink=self.active_uplink
                    )
                    await self.bus.publish(
                        UplinkEvent(
                            kind="health_changed",
                            active_uplink=self.active_uplink,
                            available=available,
                            internet_reachable=True,
                            data_cap_state=(
                                self.data_cap.get_usage()["state"]
                                if self.data_cap is not None
                                else None
                            ),
                            timestamp_ms=self._now_ms(),
                        )
                    )
                # Check whether a higher-priority uplink came back.
                if cooldown_ok and self.active_uplink is not None:
                    current_idx = (
                        self._priority.index(self.active_uplink)
                        if self.active_uplink in self._priority
                        else len(self._priority)
                    )
                    higher = [
                        u for u in available
                        if u in self._priority
                        and self._priority.index(u) < current_idx
                    ]
                    if higher:
                        self._success_streak += 1
                        if self._success_streak >= _SUCCESS_UP_THRESHOLD:
                            # Probe the higher-priority uplink before
                            # switching up so we do not drop off a
                            # working link for a dead one.
                            candidate = higher[0]
                            cand_iface = (
                                await self._uplink_iface(candidate)
                                or candidate
                            )
                            if await self._probe_host(cand_iface):
                                await self._switch_to(candidate, available)
                    else:
                        self._success_streak = 0
                return

            # Fail path.
            self._success_streak = 0
            self._fail_streak += 1
            if self._fail_streak < _FAIL_DOWN_THRESHOLD:
                return
            if not cooldown_ok:
                return

            # Pick next viable uplink below the current one.
            next_uplink: Optional[str] = None
            if self.active_uplink in self._priority:
                current_idx = self._priority.index(self.active_uplink)
                for candidate in self._priority[current_idx + 1:]:
                    if candidate in available:
                        next_uplink = candidate
                        break
            # If none below, try anything available except current.
            if next_uplink is None:
                alternatives = [u for u in available if u != self.active_uplink]
                if alternatives:
                    next_uplink = alternatives[0]

            if next_uplink is None:
                # Only the current (failing) uplink is available.
                if self.internet_reachable:
                    self.internet_reachable = False
                    await self.bus.publish(
                        UplinkEvent(
                            kind="health_changed",
                            active_uplink=self.active_uplink,
                            available=available,
                            internet_reachable=False,
                            data_cap_state=(
                                self.data_cap.get_usage()["state"]
                                if self.data_cap is not None
                                else None
                            ),
                            timestamp_ms=self._now_ms(),
                        )
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
            host=_HEALTH_HOST,
        )
        while not self._stop.is_set():
            try:
                await self._tick()
            except Exception as exc:
                log.warning("uplink.tick_error", error=str(exc))
            try:
                await asyncio.wait_for(
                    self._stop.wait(), timeout=_HEALTH_INTERVAL_SECONDS
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


# ----------------------------------------------------------------------
# Module-level singleton
# ----------------------------------------------------------------------
_instance: "UplinkRouter | None" = None


def get_uplink_router() -> "UplinkRouter":
    global _instance
    if _instance is None:
        _instance = _build_router_with_concrete_managers()
    return _instance


def _build_router_with_concrete_managers() -> "UplinkRouter":
    """Construct the router wrapping the real singleton managers.

    Imports are local so that test harnesses or callers that want a
    stub-only router can still instantiate `UplinkRouter()` directly
    without pulling in NetworkManager or ModemManager dependencies.
    """
    try:
        from ados.services.ground_station.modem_manager import (
            get_modem_manager,
        )
        from ados.services.ground_station.wifi_client_manager import (
            get_wifi_client_manager,
        )
        from ados.services.ground_station.ethernet_manager import (
            get_ethernet_manager,
        )
    except Exception as exc:
        log.warning("uplink.manager_import_failed", error=str(exc))
        return UplinkRouter()

    try:
        modem_raw = get_modem_manager()
        wifi_raw = get_wifi_client_manager()
        eth_raw = get_ethernet_manager()
    except Exception as exc:
        log.warning("uplink.manager_init_failed", error=str(exc))
        return UplinkRouter()

    return UplinkRouter(
        modem_manager=_ModemAdapter(modem_raw),
        wifi_client_manager=_WifiClientAdapter(wifi_raw),
        ethernet_manager=_EthernetAdapter(eth_raw),
    )


def _start_manager_polling() -> None:
    """Kick the periodic poll loops on the WiFi and Ethernet managers.

    The modem manager has no standalone polling loop. WiFi exposes
    `start_polling()` with a 10s cadence. Ethernet exposes
    `start_polling()` with a 5s cadence. Both are idempotent on a
    running event loop.
    """
    try:
        from ados.services.ground_station.wifi_client_manager import (
            get_wifi_client_manager,
        )
        from ados.services.ground_station.ethernet_manager import (
            get_ethernet_manager,
        )
    except Exception as exc:
        log.debug("uplink.poll_import_failed", error=str(exc))
        return

    try:
        get_wifi_client_manager().start_polling()
    except Exception as exc:
        log.warning("uplink.wifi_polling_start_failed", error=str(exc))

    try:
        get_ethernet_manager().start_polling()
    except Exception as exc:
        log.warning("uplink.eth_polling_start_failed", error=str(exc))


# ----------------------------------------------------------------------
# H9: data-cap throttle consumer
# ----------------------------------------------------------------------
async def _run_data_cap_throttle_consumer(router: "UplinkRouter") -> None:
    """Subscribe to the router bus and apply throttle on cap transitions.

    Severity ladder:
      warn_80     -> INFO  (remove any throttle, restore NAT)
      throttle_95 -> WARN  (install 256 kbit tbf qdisc on active iface)
      blocked_100 -> ERROR (drop MASQUERADE rule, hard block)

    Direct-call wiring: the consumer resolves the active uplink's
    interface at event-time by asking the router, and calls
    `share_uplink_firewall.apply_throttle`. Errors are best-effort
    logged and never crash the loop.
    """
    try:
        from ados.services.ground_station.share_uplink_firewall import (
            apply_throttle as _apply_throttle,
        )
    except Exception as exc:
        log.warning("uplink.throttle_import_failed", error=str(exc))
        return

    try:
        async for evt in router.bus.subscribe():
            if evt.kind != "data_cap_threshold":
                continue
            state = evt.data_cap_state
            if state is None:
                continue

            # Resolve active iface fresh on each transition.
            active_iface: Optional[str] = None
            active_name = router.active_uplink
            if active_name:
                try:
                    active_iface = await router._uplink_iface(active_name)  # noqa: SLF001
                except Exception as exc:
                    log.debug(
                        "uplink.throttle_iface_lookup_failed",
                        error=str(exc),
                    )

            if state == "ok":
                log.debug(
                    "uplink.datacap_throttle_applied",
                    state=state,
                    iface=active_iface,
                )
            elif state == "warn_80":
                log.info(
                    "uplink.datacap_warn_80",
                    iface=active_iface,
                    note="usage crossed 80 percent of cellular cap",
                )
            elif state == "throttle_95":
                log.warning(
                    "uplink.datacap_throttle_95",
                    iface=active_iface,
                    rate_kbps=256,
                )
            elif state == "blocked_100":
                log.error(
                    "uplink.datacap_blocked_100",
                    iface=active_iface,
                    note="cellular cap reached; NAT forwarding dropped",
                )

            try:
                result = await _apply_throttle(active_iface, state)
                log.info("uplink.datacap_throttle_result", result=result)
            except Exception as exc:
                log.warning(
                    "uplink.datacap_throttle_apply_failed",
                    state=state,
                    error=str(exc),
                )
    except asyncio.CancelledError:
        return
    except Exception as exc:
        log.warning("uplink.datacap_throttle_consumer_exc", error=str(exc))


# ----------------------------------------------------------------------
# Service entry point
# ----------------------------------------------------------------------
async def _run_service() -> None:
    router = get_uplink_router()
    _start_manager_polling()
    router.bind_manager_events()
    stop_event = asyncio.Event()

    loop = asyncio.get_running_loop()

    def _handle_signal(signame: str) -> None:
        log.info("uplink.signal_received", signal=signame)
        stop_event.set()

    for signame in ("SIGINT", "SIGTERM"):
        try:
            loop.add_signal_handler(
                getattr(signal, signame), _handle_signal, signame
            )
        except NotImplementedError:
            pass

    await router.start()
    log.info(
        "uplink.service_ready",
        priority=router.get_priority(),
        active=router.active_uplink,
    )

    # Reconcile share_uplink firewall state on start. Brings sysctl
    # ip_forward + NAT MASQUERADE in line with the persisted
    # `ground_station.share_uplink` flag so reboots survive.
    try:
        from ados.services.ground_station.share_uplink_firewall import (
            reconcile_on_start as _reconcile_share_uplink,
        )
        result = await _reconcile_share_uplink()
        log.info("uplink.share_uplink_reconciled", result=result)
    except Exception as exc:
        log.warning("uplink.share_uplink_reconcile_failed", error=str(exc))

    # H9: wire data-cap throttle consumer. Subscribes to the router's
    # own bus and calls share_uplink_firewall.apply_throttle on each
    # DataCapState transition. Direct bus subscribe matches the pattern
    # used by CloudRelayBridge and the manager-event bridges above.
    throttle_task: Optional[asyncio.Task] = None
    try:
        throttle_task = asyncio.create_task(
            _run_data_cap_throttle_consumer(router)
        )
    except Exception as exc:
        log.warning("uplink.throttle_consumer_start_failed", error=str(exc))

    await stop_event.wait()

    if throttle_task is not None:
        throttle_task.cancel()
        try:
            await throttle_task
        except (asyncio.CancelledError, Exception):
            pass

    await router.stop()
    log.info("uplink.service_stopped")


def main() -> None:
    """Systemd entry point for `ados-uplink.service`."""
    try:
        asyncio.run(_run_service())
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
