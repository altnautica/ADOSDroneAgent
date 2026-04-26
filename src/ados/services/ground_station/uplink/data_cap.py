"""Cellular data-cap tracker.

Polls the modem manager at 60 second intervals, accumulates bytes,
persists to `/var/lib/ados/modem-usage.json`, and emits threshold events
on the supplied `UplinkEventBus` when crossing 80, 95, and 100 percent
of the configured cap.
"""

from __future__ import annotations

import asyncio
import json
import os
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Optional

import structlog

from .events import DataCapState, UplinkEvent, UplinkEventBus

__all__ = [
    "_UsageState",
    "DataCapTracker",
    "_USAGE_STATE_PATH",
    "_DATA_CAP_INTERVAL_SECONDS",
    "_DEFAULT_CAP_GB",
]

log = structlog.get_logger(__name__)

_USAGE_STATE_PATH = Path("/var/lib/ados/modem-usage.json")
_DATA_CAP_INTERVAL_SECONDS = 60.0
_DEFAULT_CAP_GB = 5.0


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
    """Tracks cellular data usage across month windows."""

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
        """Atomic write of the cumulative counter.

        Writes to a temp file, fsyncs the file descriptor, then atomically
        replaces the canonical path. The fsync defends against power loss
        between write() and the kernel flushing dirty pages — without it
        the file system can drop the bytes and the cap counter rolls back.
        """
        try:
            self._state_path.parent.mkdir(parents=True, exist_ok=True)
            tmp = self._state_path.with_suffix(".json.tmp")
            payload = json.dumps(self._state.to_json()).encode("utf-8")
            with open(tmp, "wb") as fh:
                fh.write(payload)
                fh.flush()
                os.fsync(fh.fileno())
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
        """Graceful stop. Flushes the latest counter to disk before
        cancelling the poll loop so a clean shutdown does not lose the
        bytes accumulated since the last 60-second poll. SIGKILL still
        loses up to one poll window — that is inherent."""
        self._stop.set()
        if self._task is not None:
            self._task.cancel()
            try:
                await self._task
            except (asyncio.CancelledError, Exception):
                pass
            self._task = None
        try:
            self._save_state()
        except Exception as exc:
            log.warning("uplink.datacap_stop_flush_failed", error=str(exc))

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
