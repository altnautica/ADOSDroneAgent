"""WFB-ng process manager — starts/stops wfb_tx and wfb_rx subprocesses."""

from __future__ import annotations

import asyncio
from enum import StrEnum
from typing import TYPE_CHECKING

from ados.core.logging import get_logger
from ados.core.paths import WFB_KEY_DIR
from ados.services.wfb.adapter import detect_wfb_adapters, set_monitor_mode, set_tx_power
from ados.services.wfb.key_mgr import get_key_paths, key_exists
from ados.services.wfb.link_quality import LinkQualityMonitor, LinkStats

if TYPE_CHECKING:
    from ados.core.config import WfbConfig

log = get_logger("wfb.manager")


class LinkState(StrEnum):
    """WFB-ng link connection state."""

    DISCONNECTED = "disconnected"
    UNPAIRED = "unpaired"
    AUTO_PAIRING = "auto_pairing"
    BINDING = "binding"
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
        self._effective_tx_power_dbm: int | None = None
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
        # Pair state is canonically held by PairManager, which reads
        # the persisted /etc/ados/config.yaml. Read it lazily here so
        # the heartbeat reflects post-pair state without a config
        # reload race. Best-effort: failures fall through to None.
        paired = False
        peer_id: str | None = None
        paired_at: str | None = None
        fingerprint: str | None = None
        auto_pair_enabled = bool(
            getattr(self._config, "auto_pair_enabled", False)
        )
        try:
            from ados.services.ground_station.pair_manager import get_pair_manager

            pm = get_pair_manager()
            # Drone-side wfb manager reads tx.key, GS-side reads rx.key.
            # `WfbManager` is the air-side path; the GS path is
            # `ground_station.wfb_rx.WfbRxManager` (which has its own
            # `stats()` shape). Hardcoding "drone" here is correct for
            # this class.
            from ados.services.wfb.key_mgr import read_public_fingerprint

            try:
                fingerprint = read_public_fingerprint(pm.tx_key_path)
                paired = True
            except (FileNotFoundError, ValueError):
                paired = False
        except Exception:  # noqa: BLE001
            paired = False

        try:
            from ados.core.config import load_config

            cfg = load_config()
            wfb_cfg = getattr(cfg.video, "wfb", None) if cfg else None
            if wfb_cfg is not None:
                peer_id = getattr(wfb_cfg, "paired_with_device_id", None)
                paired_at = getattr(wfb_cfg, "paired_at", None)
                auto_pair_enabled = bool(
                    getattr(wfb_cfg, "auto_pair_enabled", auto_pair_enabled)
                )
        except Exception:  # noqa: BLE001
            pass

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
            "tx_power_dbm": self._effective_tx_power_dbm,
            "tx_power_max_dbm": self._config.tx_power_max_dbm,
            "mcs_index": self._config.mcs_index,
            "topology": self._config.topology,
            "paired": paired,
            "paired_with_device_id": peer_id,
            "paired_at": paired_at,
            "public_key_fingerprint": fingerprint,
            "auto_pair_enabled": auto_pair_enabled,
        }

    def force_state(self, state: LinkState) -> None:
        """Override link state externally.

        The bind orchestrator flips this to BINDING before stopping the
        wfb subprocesses and to UNPAIRED on a clean teardown so the
        heartbeat stays accurate during the bind window. Idempotent.
        """
        self._state = state

    @property
    def effective_tx_power_dbm(self) -> int | None:
        """Last effective TX power applied via iw, or None if never set."""
        return self._effective_tx_power_dbm

    def apply_tx_power(self, dbm: int) -> int | None:
        """Apply a TX power setting against the live monitor interface.

        Clamps to [1, tx_power_max_dbm]. Returns the effective dBm
        reported by the driver, or None if no interface is up yet.
        """
        if not self._interface:
            log.warning("wfb_apply_txpower_no_interface")
            return None
        clamped = max(1, min(int(dbm), self._config.tx_power_max_dbm))
        if clamped != dbm:
            log.info("wfb_txpower_clamped", requested=dbm, clamped=clamped)
        effective = set_tx_power(self._interface, clamped)
        if effective is not None:
            self._effective_tx_power_dbm = effective
            self._config.tx_power_dbm = effective
        return effective

    async def start_tx(self, interface: str, channel: int) -> bool:
        """Launch wfb_tx subprocess.

        Args:
            interface: WiFi interface in monitor mode.
            channel: Channel number to transmit on.

        Returns:
            True if the process started successfully.
        """
        tx_key, _rx_key = get_key_paths()
        # wfb_tx flags (vendored wfb-ng v26.4):
        #   -k <RS_K>  Reed-Solomon K (FEC k, default 8)
        #   -n <RS_N>  Reed-Solomon N (FEC n, default 12)
        #   -B <BW>    bandwidth in MHz (20 or 40)
        #   -M <idx>   MCS index
        # The previous code used -B for fec_k and -r for fec_n which
        # were the wrong flags entirely; -r isn't a valid option in
        # the vendored build and wfb_tx exited immediately.
        cmd = [
            "wfb_tx",
            "-p", "0",
            "-u", "5600",
            "-K", tx_key,
            "-k", str(self._config.fec_k),
            "-n", str(self._config.fec_n),
            "-B", "20",
            "-M", str(self._config.mcs_index),
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
        Blocks (does not spawn) when no encryption keypair is on disk so we
        do not produce a restart loop while the rig is waiting to pair.
        """
        self._running = True
        backoff = 1.0
        unpaired_logged = False

        while self._running:
            # Step 0: Block until the drone-side keypair (tx.key) is
            # present. The auto_pair supervisor + bind orchestrator
            # land it at WFB_KEY_DIR after a successful bind. Until
            # then, there is no point bringing up wfb_tx — it would
            # just print "key not found" and exit. WfbManager runs on
            # the drone side, so we pass role="drone".
            if not key_exists(role="drone"):
                if not unpaired_logged:
                    log.info("wfb_blocked_unpaired", expected=f"{WFB_KEY_DIR}/tx.key")
                    unpaired_logged = True
                self._state = LinkState.UNPAIRED
                await asyncio.sleep(5.0)
                continue
            unpaired_logged = False

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

            # Step 2b: Apply TX power on the monitor interface BEFORE
            # wfb_tx starts. wfb_tx itself does not set radio power; it
            # is set on the netdev via `iw set txpower fixed`. Without
            # this, the dongle runs at driver default (~17-20 dBm) and
            # browns out on host-VBUS topology within seconds of the
            # first sustained TX burst.
            effective = set_tx_power(interface, self._config.tx_power_dbm)
            self._effective_tx_power_dbm = effective
            if effective is None:
                log.warning(
                    "wfb_txpower_not_applied",
                    interface=interface,
                    requested=self._config.tx_power_dbm,
                )

            # Step 2c: Set the radio channel via iw. wfb_tx/wfb_rx
            # themselves do not change channel; they listen/transmit
            # on whatever the netdev is currently set to. Without this
            # call the radio sits on whatever the bind profile (or
            # driver default) left it at, which may not be the
            # configured wfb channel.
            try:
                import subprocess as _sp
                ch_result = _sp.run(
                    ["iw", interface, "set", "channel", str(self._channel)],
                    capture_output=True,
                    timeout=5,
                )
                if ch_result.returncode != 0:
                    log.warning(
                        "wfb_channel_set_failed",
                        interface=interface,
                        channel=self._channel,
                        stderr=ch_result.stderr.decode(errors="replace").strip(),
                    )
                else:
                    log.info(
                        "wfb_channel_set",
                        interface=interface,
                        channel=self._channel,
                    )
            except (FileNotFoundError, _sp.TimeoutExpired) as exc:
                log.warning("wfb_channel_set_error", error=str(exc))

            # Step 3 (key existence): already enforced at the top of the
            # loop. If keys disappeared between then and now, the
            # subprocess will exit on its own and we re-enter the loop.

            # Step 4: Start wfb_tx (always) and wfb_rx (only if rx.key
            # is present — on drone-only rigs we don't have rx.key and
            # wfb_rx would crash immediately, dragging the manager
            # into a restart loop that hits max_restarts and gives up).
            tx_ok = await self.start_tx(interface, self._channel)
            from pathlib import Path as _P
            from ados.core.paths import WFB_KEY_DIR as _WD
            rx_key_present = _P(str(_WD)) / "rx.key"
            rx_ok = False
            if rx_key_present.is_file():
                rx_ok = await self.start_rx(interface, self._channel)
            else:
                log.info(
                    "wfb_rx_skipped_no_rx_key",
                    note="drone-only rig; uplink RX disabled",
                )

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

            try:
                if tasks:
                    done, _pending = await asyncio.wait(tasks, return_when=asyncio.FIRST_COMPLETED)
                    # A process exited or output ended
                    for task in done:
                        task.result()  # Propagate exceptions
            finally:
                # Cancel all spawned monitoring tasks to prevent orphans
                for task in tasks:
                    if not task.done():
                        task.cancel()
                # Await cancellation so tasks clean up properly
                await asyncio.gather(*tasks, return_exceptions=True)

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
