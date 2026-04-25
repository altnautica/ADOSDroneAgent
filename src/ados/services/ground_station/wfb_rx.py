"""WFB-ng ground-side RX lifecycle.

Mirrors `ados.services.wfb.manager.WfbManager` for the ground-station
profile. On the air side we run `wfb_tx` to transmit; here we run
`wfb_rx` only, feeding decoded packets out to localhost UDP 5600 so
the mediamtx ground profile can pick them up and republish as WHEP.

Lifecycle:
1. Detect a compatible RTL8812 adapter via the shared wfb.adapter
   helper (same VID/PID table the air side uses).
2. Bring the interface up in monitor mode.
3. Launch `wfb_rx` with the same radio port, key path, and client
   UDP address conventions used by the air-side TX.
4. Tail wfb_rx stdout into LinkQualityMonitor for RSSI/FEC stats.
5. Auto-restart on process exit with exponential backoff.

Exits non-zero if no compatible adapter is detected. systemd restart
policy handles the retry loop, same pattern as the air service.
"""

from __future__ import annotations

import asyncio
import signal
import sys
from enum import StrEnum
from typing import TYPE_CHECKING

import structlog

from ados.core.config import load_config
from ados.core.logging import configure_logging, get_logger
from ados.core.paths import WFB_KEY_DIR
from ados.services.wfb.adapter import detect_wfb_adapters, set_monitor_mode
from ados.services.wfb.key_mgr import get_key_paths, key_exists
from ados.services.wfb.link_quality import LinkQualityMonitor, LinkStats

if TYPE_CHECKING:
    from ados.core.config import WfbConfig

log = get_logger("ground_station.wfb_rx")


class LinkState(StrEnum):
    """Ground-side link connection state (mirror of air LinkState)."""

    DISCONNECTED = "disconnected"
    CONNECTING = "connecting"
    CONNECTED = "connected"
    DEGRADED = "degraded"


class WfbRxManager:
    """Manages a single `wfb_rx` subprocess for the ground profile.

    Parallel to WfbManager on the air side, but only spawns the RX
    leg. TX is never started here. The class is wire-compatible with
    the air-side stats shape so the supervisor and API layer can treat
    both sides the same way.
    """

    def __init__(self, config: WfbConfig) -> None:
        self._config = config
        self._state = LinkState.DISCONNECTED
        self._rx_proc: asyncio.subprocess.Process | None = None
        self._monitor = LinkQualityMonitor()
        self._interface: str = ""
        self._channel: int = config.channel
        self._running = False
        self._restart_count = 0
        self._max_restarts = 10

    @property
    def state(self) -> LinkState:
        return self._state

    @property
    def interface(self) -> str:
        return self._interface

    @property
    def channel(self) -> int:
        return self._channel

    @property
    def monitor(self) -> LinkQualityMonitor:
        return self._monitor

    def detect_adapter(self) -> str | None:
        """Find the first WFB-ng compatible adapter in monitor-capable state.

        Returns interface name, or None if no compatible adapter is present.
        Thin wrapper over the shared wfb.adapter helper so callers outside
        this module (TUI, API, tests) have a single entry point.
        """
        if self._config.interface:
            return self._config.interface
        adapters = detect_wfb_adapters()
        compatible = [a for a in adapters if a.is_wfb_compatible and a.supports_monitor]
        if not compatible:
            return None
        chosen = compatible[0]
        log.info(
            "ground_wfb_adapter_selected",
            interface=chosen.interface_name,
            chipset=chosen.chipset,
        )
        return chosen.interface_name

    def set_monitor_mode(self, interface: str) -> bool:
        """Set the interface to monitor mode. Thin re-export for API callers."""
        return set_monitor_mode(interface)

    async def start_rx(
        self,
        interface: str,
        channel: int,
        bitrate_profile: str | None = None,
    ) -> bool:
        """Launch wfb_rx subprocess feeding decoded stream to localhost UDP.

        Args:
            interface: WiFi interface already in monitor mode.
            channel: Radio channel the drone TX is on.
            bitrate_profile: Optional tag logged for debug; wfb_rx itself
                does not take a bitrate arg, the TX side picks rate.

        Returns:
            True on successful spawn. False if the binary is missing or
            spawn fails.
        """
        _tx_key, rx_key = get_key_paths()
        cmd = [
            "wfb_rx",
            "-p", "0",
            "-c", "127.0.0.1",
            "-u", "5600",
            "-K", rx_key,
            interface,
        ]

        try:
            self._rx_proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            self._interface = interface
            self._channel = channel
            log.info(
                "ground_wfb_rx_started",
                pid=self._rx_proc.pid,
                interface=interface,
                channel=channel,
                profile=bitrate_profile or "default",
            )
            return True
        except FileNotFoundError:
            log.error("ground_wfb_rx_not_found")
            return False
        except OSError as exc:
            log.error("ground_wfb_rx_start_failed", error=str(exc))
            return False

    async def stop_rx(self) -> None:
        """Terminate the wfb_rx subprocess if alive."""
        self._running = False
        proc = self._rx_proc
        if proc is not None and proc.returncode is None:
            try:
                proc.terminate()
                await asyncio.wait_for(proc.wait(), timeout=5.0)
                log.info("ground_wfb_rx_stopped", pid=proc.pid)
            except TimeoutError:
                try:
                    proc.kill()
                except ProcessLookupError:
                    pass
                try:
                    await asyncio.wait_for(proc.wait(), timeout=1.0)
                except asyncio.TimeoutError:
                    pass
                log.warning("ground_wfb_rx_killed", pid=proc.pid)
            except ProcessLookupError:
                log.debug("ground_wfb_rx_already_exited")
        self._rx_proc = None
        self._state = LinkState.DISCONNECTED

    def stats(self) -> dict:
        """Return the ground-side link stats shape.

        Schema: rssi_dbm, bitrate_mbps, fec_rec, fec_lost, channel.
        Additional fields from the shared monitor are included for
        parity with the air-side status payload.
        """
        snap: LinkStats = self._monitor.get_current()
        return {
            "state": self._state.value,
            "interface": self._interface,
            "channel": self._channel,
            "rssi_dbm": snap.rssi_dbm,
            "bitrate_mbps": round(snap.bitrate_kbps / 1000.0, 3),
            "bitrate_kbps": snap.bitrate_kbps,
            "fec_rec": snap.fec_recovered,
            "fec_lost": snap.fec_failed,
            "packets_received": snap.packets_received,
            "packets_lost": snap.packets_lost,
            "loss_percent": snap.loss_percent,
            "snr_db": snap.snr_db,
            "restart_count": self._restart_count,
            "samples": self._monitor.sample_count,
        }

    async def _read_rx_output(self) -> None:
        """Feed wfb_rx stdout lines into the link quality monitor."""
        if self._rx_proc is None or self._rx_proc.stdout is None:
            return
        while self._running:
            try:
                line_bytes = await self._rx_proc.stdout.readline()
                if not line_bytes:
                    break
                line = line_bytes.decode("utf-8", errors="replace").strip()
                if line:
                    snap = self._monitor.feed_line(line)
                    if snap is not None:
                        self._update_state_from_stats(snap)
            except Exception as exc:
                log.debug("ground_rx_read_error", error=str(exc))
                break

    def _update_state_from_stats(self, snap: LinkStats) -> None:
        if snap.loss_percent > 50.0 or snap.rssi_dbm < -85.0:
            self._state = LinkState.DEGRADED
        elif snap.packets_received > 0:
            self._state = LinkState.CONNECTED
        else:
            self._state = LinkState.CONNECTING

    async def run(self) -> None:
        """Main service loop with adapter detection and auto-restart."""
        self._running = True
        backoff = 1.0

        while self._running:
            self._state = LinkState.CONNECTING

            interface = self.detect_adapter()
            if not interface:
                log.warning("ground_no_wfb_adapter_found")
                self._state = LinkState.DISCONNECTED
                # First-boot case: fail fast so systemd restart policy
                # can retry after a USB enumeration completes.
                if self._restart_count == 0:
                    log.error("ground_wfb_exit_no_adapter")
                    self._running = False
                    return
                await asyncio.sleep(backoff)
                backoff = min(backoff * 2, 30.0)
                continue

            self._interface = interface

            if not self.set_monitor_mode(interface):
                log.error("ground_monitor_mode_failed", interface=interface)
                self._state = LinkState.DISCONNECTED
                await asyncio.sleep(backoff)
                backoff = min(backoff * 2, 30.0)
                continue

            if not key_exists():
                log.warning("ground_wfb_keys_missing", expected=f"{WFB_KEY_DIR}/")

            rx_ok = await self.start_rx(interface, self._channel)
            if not rx_ok:
                log.error("ground_wfb_rx_failed_to_start")
                self._state = LinkState.DISCONNECTED
                self._restart_count += 1
                if self._restart_count >= self._max_restarts:
                    log.error(
                        "ground_wfb_max_restarts_reached",
                        count=self._restart_count,
                    )
                    self._running = False
                    break
                await asyncio.sleep(backoff)
                backoff = min(backoff * 2, 30.0)
                continue

            backoff = 1.0
            self._state = LinkState.CONNECTED

            tasks: list[asyncio.Task] = []
            if self._rx_proc is not None:
                tasks.append(asyncio.create_task(self._read_rx_output()))
                tasks.append(asyncio.create_task(self._rx_proc.wait()))

            try:
                if tasks:
                    done, _pending = await asyncio.wait(
                        tasks, return_when=asyncio.FIRST_COMPLETED
                    )
                    for task in done:
                        task.result()
            finally:
                for task in tasks:
                    if not task.done():
                        task.cancel()
                await asyncio.gather(*tasks, return_exceptions=True)

            if not self._running:
                break

            self._restart_count += 1
            log.warning(
                "ground_wfb_rx_exited",
                restart_count=self._restart_count,
                backoff=backoff,
            )

            await self.stop_rx()
            self._running = True

            if self._restart_count >= self._max_restarts:
                log.error(
                    "ground_wfb_max_restarts_reached",
                    count=self._restart_count,
                )
                break

            await asyncio.sleep(backoff)
            backoff = min(backoff * 2, 30.0)

        self._state = LinkState.DISCONNECTED


async def main() -> None:
    """Service entry point. Invoked by systemd via `python -m`."""
    config = load_config()
    configure_logging(config.logging.level)
    slog = structlog.get_logger()
    slog.info("ground_wfb_rx_service_starting")

    shutdown = asyncio.Event()
    loop = asyncio.get_event_loop()
    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, shutdown.set)

    manager = WfbRxManager(config.video.wfb)
    manager_task = asyncio.create_task(manager.run(), name="ground-wfb-rx")

    slog.info("ground_wfb_rx_service_ready")

    # Shut down if either signal fires or the manager returns (adapter
    # missing on first boot is a clean non-zero exit).
    done, _pending = await asyncio.wait(
        [asyncio.create_task(shutdown.wait()), manager_task],
        return_when=asyncio.FIRST_COMPLETED,
    )

    slog.info("ground_wfb_rx_service_stopping")
    manager_task.cancel()
    await asyncio.gather(manager_task, return_exceptions=True)
    await manager.stop_rx()
    slog.info("ground_wfb_rx_service_stopped")

    # Non-zero exit if the manager bailed out with no adapter.
    if manager.state == LinkState.DISCONNECTED and manager._restart_count == 0:
        sys.exit(2)


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
    sys.exit(0)
