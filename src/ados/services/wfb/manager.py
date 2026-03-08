"""WFB-ng process manager — starts/stops wfb_tx and wfb_rx subprocesses."""

from __future__ import annotations

import asyncio
from enum import StrEnum
from typing import TYPE_CHECKING

from ados.core.logging import get_logger
from ados.services.wfb.adapter import detect_wfb_adapters, set_monitor_mode
from ados.services.wfb.key_mgr import get_key_paths, key_exists
from ados.services.wfb.link_quality import LinkQualityMonitor, LinkStats

if TYPE_CHECKING:
    from ados.core.config import WfbConfig

log = get_logger("wfb.manager")


class LinkState(StrEnum):
    """WFB-ng link connection state."""

    DISCONNECTED = "disconnected"
    CONNECTING = "connecting"
    CONNECTED = "connected"
    DEGRADED = "degraded"


class WfbManager:
    """Manages wfb_tx and wfb_rx subprocesses for WFB-ng video link.

    Lifecycle:
    1. Detect compatible WiFi adapter
    2. Set adapter to monitor mode
    3. Launch wfb_tx and wfb_rx as subprocesses
    4. Monitor link quality from wfb_rx stdout
    5. Auto-restart on process exit with exponential backoff
    """

    def __init__(self, config: WfbConfig) -> None:
        self._config = config
        self._state = LinkState.DISCONNECTED
        self._tx_proc: asyncio.subprocess.Process | None = None
        self._rx_proc: asyncio.subprocess.Process | None = None
        self._monitor = LinkQualityMonitor()
        self._interface: str = ""
        self._channel: int = config.channel
        self._running = False
        self._restart_count = 0
        self._max_restarts = 10

    @property
    def state(self) -> LinkState:
        """Current link state."""
        return self._state

    @property
    def interface(self) -> str:
        """Active WiFi interface name."""
        return self._interface

    @property
    def channel(self) -> int:
        """Active channel number."""
        return self._channel

    @property
    def monitor(self) -> LinkQualityMonitor:
        """Link quality monitor with stats history."""
        return self._monitor

    def get_status(self) -> dict:
        """Get current link status as a dictionary."""
        stats = self._monitor.get_current()
        return {
            "state": self._state.value,
            "interface": self._interface,
            "channel": self._channel,
            "rssi_dbm": stats.rssi_dbm,
            "noise_dbm": stats.noise_dbm,
            "snr_db": stats.snr_db,
            "packets_received": stats.packets_received,
            "packets_lost": stats.packets_lost,
            "loss_percent": stats.loss_percent,
            "fec_recovered": stats.fec_recovered,
            "fec_failed": stats.fec_failed,
            "bitrate_kbps": stats.bitrate_kbps,
            "restart_count": self._restart_count,
            "samples": self._monitor.sample_count,
        }

    async def start_tx(self, interface: str, channel: int) -> bool:
        """Launch wfb_tx subprocess.

        Args:
            interface: WiFi interface in monitor mode.
            channel: Channel number to transmit on.

        Returns:
            True if the process started successfully.
        """
        tx_key, _rx_key = get_key_paths()
        cmd = [
            "wfb_tx",
            "-p", "0",
            "-u", "5600",
            "-K", tx_key,
            "-B", str(self._config.fec_k),
            "-r", str(self._config.fec_n),
            "-M", str(self._config.tx_power),
            interface,
        ]

        try:
            self._tx_proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            log.info("wfb_tx_started", pid=self._tx_proc.pid, channel=channel)
            return True
        except FileNotFoundError:
            log.error("wfb_tx_not_found")
            return False
        except OSError as e:
            log.error("wfb_tx_start_failed", error=str(e))
            return False

    async def start_rx(self, interface: str, channel: int) -> bool:
        """Launch wfb_rx subprocess.

        Args:
            interface: WiFi interface in monitor mode.
            channel: Channel number to receive on.

        Returns:
            True if the process started successfully.
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
            log.info("wfb_rx_started", pid=self._rx_proc.pid, channel=channel)
            return True
        except FileNotFoundError:
            log.error("wfb_rx_not_found")
            return False
        except OSError as e:
            log.error("wfb_rx_start_failed", error=str(e))
            return False

    async def stop(self) -> None:
        """Terminate wfb_tx and wfb_rx subprocesses."""
        self._running = False

        for name, proc in [("wfb_tx", self._tx_proc), ("wfb_rx", self._rx_proc)]:
            if proc is not None and proc.returncode is None:
                try:
                    proc.terminate()
                    await asyncio.wait_for(proc.wait(), timeout=5.0)
                    log.info("wfb_process_stopped", name=name, pid=proc.pid)
                except TimeoutError:
                    proc.kill()
                    log.warning("wfb_process_killed", name=name, pid=proc.pid)
                except ProcessLookupError:
                    log.debug("wfb_process_already_exited", name=name)

        self._tx_proc = None
        self._rx_proc = None
        self._state = LinkState.DISCONNECTED
        log.info("wfb_stopped")

    async def _read_rx_output(self) -> None:
        """Read wfb_rx stdout and feed lines to the link quality monitor."""
        if self._rx_proc is None or self._rx_proc.stdout is None:
            return

        while self._running:
            try:
                line_bytes = await self._rx_proc.stdout.readline()
                if not line_bytes:
                    break
                line = line_bytes.decode("utf-8", errors="replace").strip()
                if line:
                    stats = self._monitor.feed_line(line)
                    if stats is not None:
                        self._update_state_from_stats(stats)
            except Exception as e:
                log.debug("rx_read_error", error=str(e))
                break

    def _update_state_from_stats(self, stats: LinkStats) -> None:
        """Update link state based on current statistics."""
        if stats.loss_percent > 50.0 or stats.rssi_dbm < -85.0:
            self._state = LinkState.DEGRADED
        elif stats.packets_received > 0:
            self._state = LinkState.CONNECTED
        else:
            self._state = LinkState.CONNECTING

    async def run(self) -> None:
        """Main service loop: detect adapter, set monitor mode, start wfb processes.

        Auto-restarts failed processes with exponential backoff up to max_restarts.
        """
        self._running = True
        backoff = 1.0

        while self._running:
            self._state = LinkState.CONNECTING

            # Step 1: Find a compatible adapter
            interface = self._config.interface
            if not interface:
                adapters = detect_wfb_adapters()
                compatible = [a for a in adapters if a.is_wfb_compatible and a.supports_monitor]
                if not compatible:
                    log.warning("no_wfb_adapter_found", total_adapters=len(adapters))
                    self._state = LinkState.DISCONNECTED
                    await asyncio.sleep(backoff)
                    backoff = min(backoff * 2, 30.0)
                    continue
                interface = compatible[0].interface_name
                log.info("wfb_adapter_selected", interface=interface, chipset=compatible[0].chipset)

            self._interface = interface

            # Step 2: Set monitor mode
            if not set_monitor_mode(interface):
                log.error("monitor_mode_failed", interface=interface)
                self._state = LinkState.DISCONNECTED
                await asyncio.sleep(backoff)
                backoff = min(backoff * 2, 30.0)
                continue

            # Step 3: Check for encryption keys
            if not key_exists():
                log.warning("wfb_keys_missing", expected="/etc/ados/wfb/")

            # Step 4: Start wfb_tx and wfb_rx
            tx_ok = await self.start_tx(interface, self._channel)
            rx_ok = await self.start_rx(interface, self._channel)

            if not tx_ok and not rx_ok:
                log.error("wfb_both_failed_to_start")
                self._state = LinkState.DISCONNECTED
                self._restart_count += 1
                if self._restart_count >= self._max_restarts:
                    log.error("wfb_max_restarts_reached", count=self._restart_count)
                    self._running = False
                    break
                await asyncio.sleep(backoff)
                backoff = min(backoff * 2, 30.0)
                continue

            backoff = 1.0
            self._state = LinkState.CONNECTED

            # Step 5: Monitor processes and read stats
            tasks: list[asyncio.Task] = []
            if self._rx_proc is not None:
                tasks.append(asyncio.create_task(self._read_rx_output()))
            if self._tx_proc is not None and self._tx_proc.returncode is None:
                tasks.append(asyncio.create_task(self._tx_proc.wait()))
            if self._rx_proc is not None and self._rx_proc.returncode is None:
                tasks.append(asyncio.create_task(self._rx_proc.wait()))

            if tasks:
                done, _pending = await asyncio.wait(tasks, return_when=asyncio.FIRST_COMPLETED)
                # A process exited or output ended
                for task in done:
                    task.result()  # Propagate exceptions

            if not self._running:
                break

            # Process exited unexpectedly, restart
            self._restart_count += 1
            log.warning(
                "wfb_process_exited",
                restart_count=self._restart_count,
                backoff=backoff,
            )

            # Clean up before restart
            await self.stop()
            self._running = True  # stop() sets _running=False, re-enable

            if self._restart_count >= self._max_restarts:
                log.error("wfb_max_restarts_reached", count=self._restart_count)
                break

            await asyncio.sleep(backoff)
            backoff = min(backoff * 2, 30.0)

        self._state = LinkState.DISCONNECTED
