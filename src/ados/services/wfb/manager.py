"""WFB-ng process manager — starts/stops wfb_tx and wfb_rx subprocesses."""

from __future__ import annotations

import asyncio
import os
import time
from enum import StrEnum
from typing import TYPE_CHECKING, Any

from ados.core.logging import get_logger
from ados.core.paths import WFB_KEY_DIR
from ados.services.wfb.adapter import detect_wfb_adapters, set_monitor_mode, set_tx_power
from ados.services.wfb.key_mgr import get_key_paths, key_exists
from ados.services.wfb.link_quality import LinkQualityMonitor, LinkStats

# TX-liveness watchdog tunables. wfb_tx can be alive (returncode is None)
# but stop pushing 802.11 frames — observed after rapid bind/unbind
# cycles and monitor-mode driver wedges. Polling /sys/class/net counter
# is the only reliable way to see this from userspace; wfb_tx itself
# does not emit a "tx_byte_count" line on its stdout.
_TX_HEALTH_POLL_INTERVAL_S = 5.0
_TX_HEALTH_SILENCE_THRESHOLD_S = 30.0
# wfb_tx binds UDP 5600 for the air-side ingress feed. /proc/net/udp's
# local_address column suffixes the port as 4-char uppercase hex.
_WFB_TX_INGRESS_PORT_HEX = "15E0"
# Suppress duplicate "upstream silent" warnings to once every 5 minutes
# so a bench drone with no encoder running does not flood the journal.
_UPSTREAM_SILENT_LOG_INTERVAL_S = 300.0

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


# Operator-facing radio link presets. Map each preset to the trio of
# wfb-ng tunables (MCS index, Reed-Solomon K, Reed-Solomon N). Tuned
# for RTL8812EU radios on a 20 MHz channel; rough capacity estimates
# in comments.
_LINK_PRESETS: dict[str, tuple[int, int, int]] = {
    # MCS=1 (~13 Mbps cap, 50% redundancy). Default. Robust under low
    # SNR / host-vbus power budgets / a noisy bench.
    "conservative": (1, 8, 12),
    # MCS=3 (~26 Mbps cap, 50% redundancy). Headroom for outdoor links
    # where SNR is reliably above ~10 dB.
    "balanced": (3, 8, 12),
    # MCS=5 (~52 Mbps cap, 25% redundancy). Excellent SNR + close-in.
    # Will lose the link on a noisy channel; reduce to balanced if FEC
    # losses appear.
    "aggressive": (5, 8, 10),
}


def _apply_link_preset(config: WfbConfig, logger: Any) -> None:
    """Override mcs_index / fec_k / fec_n from the wfb_link_preset field.

    The preset is operator-facing on /etc/ados/config.yaml. Default
    "conservative" leaves the existing config values alone, which lets
    a rig with explicitly-tuned values keep them untouched. Any other
    preset value forces the trio so the operator can widen the link
    by changing one field instead of three.
    """
    preset = getattr(config, "wfb_link_preset", "conservative")
    if preset == "conservative":
        # Respect the explicit config; do not override.
        return
    spec = _LINK_PRESETS.get(preset)
    if spec is None:
        logger.warning(
            "wfb_link_preset_unknown",
            preset=preset,
            note="falling back to current config values",
        )
        return
    mcs, fec_k, fec_n = spec
    config.mcs_index = mcs
    config.fec_k = fec_k
    config.fec_n = fec_n
    logger.info(
        "wfb_link_preset_applied",
        preset=preset,
        mcs_index=mcs,
        fec_k=fec_k,
        fec_n=fec_n,
    )


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
        # Apply the link preset to mcs_index / fec_k / fec_n. Only
        # touches the config when the preset is not "conservative" so
        # an existing rig with custom values keeps them. Logged so the
        # operator can see which values are actually in flight.
        _apply_link_preset(self._config, log)
        self._state = LinkState.DISCONNECTED
        self._tx_proc: asyncio.subprocess.Process | None = None
        self._rx_proc: asyncio.subprocess.Process | None = None
        # Control-plane subprocesses on wfb-ng radio_id 1, used by the
        # FHSS hop coordinator so HopAnnounce/HopAck flow over the WFB
        # radio link itself rather than over the LAN. ADOS is a local-
        # first field-deployed agent; consumer APs frequently do not
        # bridge L2 broadcasts wired<->wireless, so any inter-rig packet
        # whose loss breaks the radio must travel via the radio.
        self._tx_control_proc: asyncio.subprocess.Process | None = None
        self._rx_control_proc: asyncio.subprocess.Process | None = None
        self._monitor = LinkQualityMonitor()
        self._interface: str = ""
        self._channel: int = config.channel
        self._effective_tx_power_dbm: int | None = None
        self._running = False
        self._restart_count = 0
        self._max_restarts = 10
        # TX-liveness watchdog state. Updated each time the radio iface
        # tx_bytes counter changes; surfaced via stats() so the heartbeat
        # carries it.
        self._last_tx_byte_value: int = -1
        self._last_tx_byte_change_at: float = 0.0
        self._tx_zombie_kills: int = 0
        # Upstream-feed tracker. Distinguishes "wfb_tx is wedged" (kill)
        # from "no video is arriving at wfb_tx's UDP 5600 socket" (don't
        # kill — wfb_tx is correctly idle when there is nothing to send).
        # Reads drops + rx_queue from /proc/net/udp for the bound port.
        # _last_upstream_byte_value tracks the drops counter (the only
        # monotone receive-side signal /proc/net/udp exposes); a non-zero
        # rx_queue at sample time is the secondary "is feeding right now"
        # hint.
        self._last_upstream_byte_value: int = -1
        self._last_upstream_change_at: float = 0.0
        # Throttle the "upstream silent" log to once every 5 minutes so a
        # drone parked on the bench with no encoder doesn't flood the
        # journal.
        self._last_upstream_silent_log_at: float = 0.0
        # Control-plane (-p 1) reception state. wfb_rx_control delivers
        # both the GS's HopAck echoes and its PresenceBeacons here.
        # `_ack_events` maps target_channel -> asyncio.Event the
        # HopSupervisor registered before sending its announce. The
        # listener fires the matching event when a HopAck for that
        # target arrives. `_peer_*` mirrors the GS-side HopListener
        # peer cache so the drone's heartbeat can surface the GS's
        # device-id after the radio link delivers one beacon.
        self._ack_events: dict[int, asyncio.Event] = {}
        self._peer_device_id: str | None = None
        self._peer_role: str | None = None
        self._peer_channel: int | None = None
        self._peer_rssi_dbm: int | None = None
        self._peer_last_seen_unix: float | None = None
        self._control_plane_listener_running: bool = False

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
            "tx_zombie_kills": self._tx_zombie_kills,
            "tx_silent_seconds": (
                round(time.monotonic() - self._last_tx_byte_change_at, 1)
                if self._last_tx_byte_change_at > 0
                else None
            ),
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
        # `-l 1000` enables wfb_rx's per-second stats line emission so
        # _read_rx_output() can feed the LinkQualityMonitor. Without
        # it wfb_rx is silent on stdout, the API surface stays at
        # default/zero values, and the LinkStatsPage on the LCD shows
        # an empty radio band even when frames are flowing.
        cmd = [
            "wfb_rx",
            "-p", "0",
            "-c", "127.0.0.1",
            "-u", "5600",
            "-K", rx_key,
            "-l", "1000",
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

    async def start_tx_control(self, interface: str, channel: int) -> bool:
        """Launch wfb_tx_control subprocess for the control-plane stream.

        The control plane (radio_id 1) carries HopAnnounce frames from
        the FHSS coordinator to the ground station. Tiny payload
        (51 bytes), lighter FEC than video (k=1, n=2), so a hop can
        be coordinated in a few hundred ms with low overhead. Reuses
        the same tx.key — wfb-ng key files contain both halves of the
        crypto_box keypair so the same file authenticates both
        directions on this and the bind streams.

        Sibling subprocess to start_tx; lifecycle is managed by the
        same supervisor and torn down by stop() alongside the data
        plane.
        """
        tx_key, _rx_key = get_key_paths()
        cmd = [
            "wfb_tx",
            "-p", "1",
            "-u", "5803",
            "-K", tx_key,
            "-k", "1",
            "-n", "2",
            "-B", "20",
            "-M", str(self._config.mcs_index),
            interface,
        ]
        # stdout=DEVNULL avoids PKT-stats buffer fill. stderr -> log
        # file (truncated on each restart) so wfb-ng diagnostics survive
        # without risking the PIPE deadlock. fd is duped by the kernel
        # for the child; we close our copy after spawn.
        stderr_fd = os.open(
            "/run/ados/wfb-drone-tx-control.log",
            os.O_WRONLY | os.O_CREAT | os.O_TRUNC,
            0o644,
        )
        try:
            self._tx_control_proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=stderr_fd,
            )
            log.info(
                "wfb_tx_control_started",
                pid=self._tx_control_proc.pid,
                channel=channel,
            )
            return True
        except FileNotFoundError:
            log.error("wfb_tx_control_not_found")
            return False
        except OSError as e:
            log.error("wfb_tx_control_start_failed", error=str(e))
            return False
        finally:
            os.close(stderr_fd)

    async def start_rx_control(self, interface: str, channel: int) -> bool:
        """Launch wfb_rx_control subprocess to receive HopAck frames.

        The control plane is bidirectional: drone -> GS carries
        HopAnnounce on this radio_id 1 stream, GS -> drone carries
        HopAck on the same stream id but the reverse direction.
        The receive end of HopAck lands on UDP 5810 where the
        HopSupervisor listens.
        """
        tx_key, _rx_key = get_key_paths()
        cmd = [
            "wfb_rx",
            "-p", "1",
            "-c", "127.0.0.1",
            "-u", "5810",
            "-K", tx_key,
            "-l", "1000",
            interface,
        ]
        # stdout=DEVNULL; stderr -> log file for wfb-ng diagnostics.
        stderr_fd = os.open(
            "/run/ados/wfb-drone-rx-control.log",
            os.O_WRONLY | os.O_CREAT | os.O_TRUNC,
            0o644,
        )
        try:
            self._rx_control_proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=stderr_fd,
            )
            log.info(
                "wfb_rx_control_started",
                pid=self._rx_control_proc.pid,
                channel=channel,
            )
            return True
        except FileNotFoundError:
            log.error("wfb_rx_control_not_found")
            return False
        except OSError as e:
            log.error("wfb_rx_control_start_failed", error=str(e))
            return False

    async def stop(self) -> None:
        """Terminate wfb_tx, wfb_rx, and the control-plane subprocesses."""
        self._running = False

        procs_to_stop = [
            ("wfb_tx", self._tx_proc),
            ("wfb_rx", self._rx_proc),
            ("wfb_tx_control", self._tx_control_proc),
            ("wfb_rx_control", self._rx_control_proc),
        ]
        for name, proc in procs_to_stop:
            if proc is not None and proc.returncode is None:
                try:
                    proc.terminate()
                    await asyncio.wait_for(proc.wait(), timeout=5.0)
                    log.info("wfb_process_stopped", name=name, pid=proc.pid)
                except TimeoutError:
                    try:
                        proc.kill()
                    except ProcessLookupError:
                        pass
                    # Reap the killed process so it doesn't sit as a
                    # zombie until the next event-loop iteration. Mirrors
                    # the GS-side stop_rx() pattern.
                    try:
                        await asyncio.wait_for(proc.wait(), timeout=1.0)
                    except asyncio.TimeoutError:
                        pass
                    log.warning("wfb_process_killed", name=name, pid=proc.pid)
                except ProcessLookupError:
                    log.debug("wfb_process_already_exited", name=name)

        self._tx_proc = None
        self._rx_proc = None
        self._tx_control_proc = None
        self._rx_control_proc = None
        self._state = LinkState.DISCONNECTED
        log.info("wfb_stopped")

    async def set_fec(self, fec_k: int, fec_n: int) -> bool:
        """Apply a new Reed-Solomon (K, N) ratio to the live wfb_tx.

        wfb_tx does not expose a runtime knob so the only correct
        application is stop + start. Cost is ~500 ms of radio
        blackout, absorbed by the GCS WHEP jitter buffer and the
        LCD's last-frame-hold. Returns False on validation or
        spawn failure; the live link is left on whatever was
        running before so a bad call does not knock the rig off
        the air.

        Wired from the BitrateController on tier-change decisions.
        Also surfaced via the REST /api/video/config write so an
        operator can pin a manual ratio.
        """
        if fec_k <= 0 or fec_n <= fec_k:
            log.warning("set_fec_invalid", k=fec_k, n=fec_n)
            return False
        if not self._interface:
            log.warning("set_fec_no_interface")
            return False
        if self._tx_proc is None or self._tx_proc.returncode is not None:
            # No live tx to restart; just persist the new values so
            # the next start_tx picks them up.
            self._config.fec_k = fec_k
            self._config.fec_n = fec_n
            return True

        old_k, old_n = self._config.fec_k, self._config.fec_n
        log.info("set_fec_applying", old_k=old_k, old_n=old_n, k=fec_k, n=fec_n)

        # Terminate the existing wfb_tx cleanly.
        try:
            self._tx_proc.terminate()
            await asyncio.wait_for(self._tx_proc.wait(), timeout=2.0)
        except TimeoutError:
            try:
                self._tx_proc.kill()
            except ProcessLookupError:
                pass
        except ProcessLookupError:
            pass
        self._tx_proc = None

        # Mutate config and respawn. start_tx reads from
        # self._config, so the update is reflected on respawn.
        self._config.fec_k = fec_k
        self._config.fec_n = fec_n
        ok = await self.start_tx(self._interface, self._channel or 0)
        if not ok:
            # Roll the config back so the in-memory state matches
            # the active wfb_tx (still nothing here, but next
            # respawn attempt should not silently keep the
            # rejected values).
            self._config.fec_k = old_k
            self._config.fec_n = old_n
            log.warning("set_fec_respawn_failed", k=fec_k, n=fec_n)
            return False
        log.info("set_fec_applied", k=fec_k, n=fec_n)
        return True

    async def set_mcs(self, mcs: int) -> bool:
        """Apply a new MCS index to the live wfb_tx.

        Same restart-on-change pattern as set_fec. The supported
        MCS range on RTL8812EU is 0..7 for VHT80; we accept 0..7
        and let the underlying wfb_tx reject anything wider.
        """
        if not 0 <= mcs <= 7:
            log.warning("set_mcs_out_of_range", mcs=mcs)
            return False
        if not self._interface:
            log.warning("set_mcs_no_interface")
            return False
        if self._tx_proc is None or self._tx_proc.returncode is not None:
            self._config.mcs_index = mcs
            return True

        old_mcs = self._config.mcs_index
        log.info("set_mcs_applying", old=old_mcs, new=mcs)
        try:
            self._tx_proc.terminate()
            await asyncio.wait_for(self._tx_proc.wait(), timeout=2.0)
        except TimeoutError:
            try:
                self._tx_proc.kill()
            except ProcessLookupError:
                pass
        except ProcessLookupError:
            pass
        self._tx_proc = None

        self._config.mcs_index = mcs
        ok = await self.start_tx(self._interface, self._channel or 0)
        if not ok:
            self._config.mcs_index = old_mcs
            log.warning("set_mcs_respawn_failed", mcs=mcs)
            return False
        log.info("set_mcs_applied", mcs=mcs)
        return True

    async def _read_rx_output(self) -> None:
        """Read wfb_rx stdout and feed lines to the link quality monitor."""
        if self._rx_proc is None or self._rx_proc.stdout is None:
            return

        from ados.core.paths import WFB_STATS_JSON

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
                        # Surface live stats to /run/ados/wfb-stats.json
                        # so the API process + OLED dashboard tile +
                        # LCD link stats page can read them. Atomic
                        # tmpfile+rename inside persist_to_file.
                        self._monitor.persist_to_file(
                            WFB_STATS_JSON,
                            extra={
                                "interface": self._interface,
                                "channel": self._channel,
                                "tx_power_dbm": self._effective_tx_power_dbm,
                                "topology": self._config.topology,
                                "mcs_index": self._config.mcs_index,
                                "profile": "drone",
                            },
                        )
            except Exception as e:
                log.debug("rx_read_error", error=str(e))
                break

    def _read_wfb_tx_upstream_state(self) -> tuple[int, int] | None:
        """Read (rx_queue, drops) from /proc/net/udp for wfb_tx's ingress.

        Returns ``None`` when the socket row cannot be located (wfb_tx
        not yet bound, /proc not present, file unreadable) so the caller
        treats it as "unknown" rather than as a definitive feed signal.

        `/proc/net/udp` columns: ``sl local_address rem_address st
        tx_queue:rx_queue tr:tm->when retrnsmt uid timeout inode ref
        pointer drops``. The bound port matches the suffix of
        ``local_address`` so this works whether wfb_tx bound to
        ``0.0.0.0:5600`` or ``127.0.0.1:5600``. Userspace cumulative
        receive-byte counters aren't exposed in /proc/net/udp; ``drops``
        is the only monotone receive-side signal here and
        ``rx_queue`` is the current depth (zero when wfb_tx has drained
        everything or when nothing arrived).
        """
        try:
            with open("/proc/net/udp") as f:
                lines = f.readlines()
        except OSError:
            return None
        for raw in lines[1:]:  # skip the header
            cols = raw.split()
            if len(cols) < 13:
                continue
            local_addr = cols[1]
            if not local_addr.endswith(":" + _WFB_TX_INGRESS_PORT_HEX):
                continue
            queues = cols[4].split(":")
            if len(queues) != 2:
                continue
            try:
                rx_queue = int(queues[1], 16)
                drops = int(cols[12])
            except ValueError:
                return None
            return (rx_queue, drops)
        return None

    def _persist_peer_presence(self) -> None:
        """Mirror HopListener._persist_peer_presence on the drone side."""
        try:
            import json as _json
            from ados.core.paths import PEER_PRESENCE_JSON
            payload = {
                "peer_device_id": self._peer_device_id,
                "peer_role": self._peer_role,
                "peer_channel": self._peer_channel,
                "peer_rssi_dbm": self._peer_rssi_dbm,
                "peer_last_seen_unix": self._peer_last_seen_unix,
            }
            tmp = PEER_PRESENCE_JSON.with_suffix(".tmp")
            tmp.parent.mkdir(parents=True, exist_ok=True)
            tmp.write_text(_json.dumps(payload))
            tmp.replace(PEER_PRESENCE_JSON)
        except OSError as exc:
            log.debug("peer_presence_persist_failed", error=str(exc))

    async def _control_plane_listener(self) -> None:
        """Long-running drone-side listener on the control-plane return port.

        wfb_rx_control writes every decoded -p 1 frame to UDP 5810 on
        the drone — both the GS's HopAck echoes (in response to a
        local HopAnnounce) and the GS's PresenceBeacons (every 10 s).
        This task binds 0.0.0.0:5810 and dispatches by magic prefix:

        * HopAnnounce echo → set the matching event in `_ack_events`
          so the supervisor's pending hop-attempt unblocks.
        * PresenceBeacon → update the peer cache + persist
          /run/ados/peer-presence.json so the heartbeat builder can
          surface the GS device-id, role, channel, RSSI, and
          freshness to Mission Control.
        """
        try:
            import socket as _socket
            from ados.core.identity import get_or_create_device_id
            from ados.services.wfb.hop_supervisor import (
                HopAnnounce,
                PresenceBeacon,
                _PRESENCE_MAGIC,
                _resolve_pair_key,
            )
        except ImportError as exc:
            log.warning("control_plane_listener_disabled", error=str(exc))
            return

        sock = _socket.socket(_socket.AF_INET, _socket.SOCK_DGRAM)
        try:
            sock.setsockopt(_socket.SOL_SOCKET, _socket.SO_REUSEADDR, 1)
            sock.bind(("0.0.0.0", 5810))
            sock.setblocking(False)
        except OSError as exc:
            log.warning("control_plane_listener_bind_failed", error=str(exc))
            sock.close()
            return

        own_device_id = ""
        try:
            own_device_id = get_or_create_device_id()
        except Exception:
            own_device_id = ""

        self._control_plane_listener_running = True
        log.info("control_plane_listener_started", port=5810)
        loop = asyncio.get_running_loop()
        try:
            while self._running:
                try:
                    data = await asyncio.wait_for(
                        loop.sock_recv(sock, 256), timeout=1.0
                    )
                except (asyncio.TimeoutError, OSError):
                    continue
                pair_key = _resolve_pair_key()
                if data.startswith(_PRESENCE_MAGIC):
                    beacon = PresenceBeacon.decode(data, pair_key)
                    if beacon is None:
                        continue
                    if own_device_id and beacon.device_id == own_device_id[:16]:
                        continue
                    previous = self._peer_device_id
                    self._peer_device_id = beacon.device_id
                    self._peer_role = beacon.role_label
                    self._peer_channel = int(beacon.channel)
                    self._peer_rssi_dbm = int(beacon.rssi_dbm)
                    self._peer_last_seen_unix = time.time()
                    if previous != beacon.device_id:
                        log.info(
                            "control_plane_peer_seen",
                            peer_device_id=beacon.device_id,
                            peer_role=beacon.role_label,
                            channel=beacon.channel,
                            rssi_dbm=beacon.rssi_dbm,
                        )
                    self._persist_peer_presence()
                    continue

                announce = HopAnnounce.decode(data, pair_key)
                if announce is None:
                    continue
                event = self._ack_events.get(announce.target_channel)
                if event is not None and not event.is_set():
                    event.set()
        finally:
            self._control_plane_listener_running = False
            try:
                sock.close()
            except OSError:
                pass

    async def _presence_emit_loop(self) -> None:
        """Periodically emit a PresenceBeacon on the WFB control plane.

        Writes a PresenceBeacon to 127.0.0.1:5803 (wfb_tx_control's
        loopback ingress on radio_id 1) every 10 s. Same crypto + key
        derivation as HopAnnounce, disambiguated by magic prefix.
        Replaces the mDNS dependency for inter-rig peer discovery so
        the GS can populate `paired_with_device_id` from the actual
        radio link rather than a local-network name lookup.
        """
        try:
            import socket as _socket
            from ados.core.identity import get_or_create_device_id
            from ados.services.wfb.hop_supervisor import (
                PresenceBeacon,
                _PRESENCE_ROLE_DRONE,
                _PRESENCE_VERSION_CURRENT,
                _resolve_pair_key,
            )
        except ImportError as exc:
            log.warning("presence_emit_disabled", error=str(exc))
            return

        device_id = get_or_create_device_id()
        sock = _socket.socket(_socket.AF_INET, _socket.SOCK_DGRAM)
        try:
            sock.bind(("0.0.0.0", 0))
            sock.setblocking(False)
        except OSError as exc:
            log.warning("presence_emit_socket_failed", error=str(exc))
            sock.close()
            return
        log.info("presence_emit_started", device_id=device_id, cadence_s=10)
        try:
            while self._running:
                try:
                    pair_key = _resolve_pair_key()
                    beacon = PresenceBeacon(
                        version=_PRESENCE_VERSION_CURRENT,
                        device_id=device_id,
                        role=_PRESENCE_ROLE_DRONE,
                        channel=int(self._channel or 0),
                        rssi_dbm=0,
                        epoch_ms=int(time.time() * 1000),
                    )
                    payload = beacon.encode(pair_key)
                    sock.sendto(payload, ("127.0.0.1", 5803))
                except OSError as exc:
                    log.debug("presence_emit_send_failed", error=str(exc))
                except Exception as exc:
                    log.warning("presence_emit_unexpected", error=str(exc))
                try:
                    await asyncio.sleep(10.0)
                except asyncio.CancelledError:
                    return
        finally:
            try:
                sock.close()
            except OSError:
                pass

    async def _tx_health_watchdog(self) -> None:
        """Catch wfb_tx zombies (process alive, radio iface tx_bytes flat).

        Polls /sys/class/net/<iface>/statistics/tx_bytes every 5 s. If
        the counter is unchanged for 30 s while wfb_tx's process is
        still alive AND its UDP 5600 ingress socket is being fed,
        terminate wfb_tx so the standard restart loop in run() respawns
        it.

        When tx_bytes is flat but the ingress is also silent (no encoder
        pushing video), do NOT terminate wfb_tx — it is correctly idle
        because there is nothing to send. Log the upstream-silent
        condition once every 5 minutes and back off.

        Covers two distinct failure modes:

        * Driver wedged: bytes arrive on UDP 5600, wfb_tx consumes them,
          but the 802.11 frames never leave the radio. Kill + respawn.
        * Encoder absent: nothing is arriving on UDP 5600. tx_bytes will
          be flat as a consequence; restarting wfb_tx changes nothing.

        Process-liveness is necessary but never sufficient for radio
        work — see operating rule 37. Modeled on RubyFPV's shared-memory
        watchdog and OpenHD's link-statistics polling.
        """
        if not self._interface or self._tx_proc is None:
            return
        counter_path = (
            f"/sys/class/net/{self._interface}/statistics/tx_bytes"
        )
        # Reset window state so we don't carry over "silent for X s"
        # across restarts of the wfb_tx process.
        self._last_tx_byte_value = -1
        self._last_tx_byte_change_at = time.monotonic()
        self._last_upstream_byte_value = -1
        self._last_upstream_change_at = time.monotonic()
        while (
            self._running
            and self._tx_proc is not None
            and self._tx_proc.returncode is None
        ):
            try:
                await asyncio.sleep(_TX_HEALTH_POLL_INTERVAL_S)
            except asyncio.CancelledError:
                return
            try:
                with open(counter_path) as f:
                    current = int(f.read().strip())
            except (FileNotFoundError, ValueError, OSError) as exc:
                # Iface gone (USB unplug) or counter unreadable. Don't
                # kill wfb_tx on that — let the stdin-pipe error or the
                # adapter detect loop in run() catch it.
                log.debug(
                    "wfb_tx_health_read_failed",
                    path=counter_path,
                    error=str(exc),
                )
                continue
            if current != self._last_tx_byte_value:
                self._last_tx_byte_value = current
                self._last_tx_byte_change_at = time.monotonic()
                continue
            silent_for = time.monotonic() - self._last_tx_byte_change_at
            if silent_for < _TX_HEALTH_SILENCE_THRESHOLD_S:
                continue

            # tx_bytes has been flat for the silence window. Look at the
            # ingress socket on UDP 5600 to decide whether wfb_tx is
            # wedged (data arriving but not transmitted) or just idle
            # (no encoder feeding it).
            upstream = self._read_wfb_tx_upstream_state()
            now = time.monotonic()
            upstream_feeding = False
            if upstream is not None:
                rx_queue, drops = upstream
                if self._last_upstream_byte_value == -1:
                    self._last_upstream_byte_value = drops
                    self._last_upstream_change_at = now
                if drops != self._last_upstream_byte_value:
                    self._last_upstream_byte_value = drops
                    self._last_upstream_change_at = now
                    upstream_feeding = True
                elif rx_queue > 0:
                    # Current queue depth proves data IS arriving at
                    # this sample, even if no overflow drop happened.
                    self._last_upstream_change_at = now
                    upstream_feeding = True

            if not upstream_feeding:
                # Upstream silent OR unknown (couldn't read /proc/net/udp).
                # Either way wfb_tx is doing nothing because there is
                # nothing to send; killing it changes nothing and would
                # cascade through stop() to the control plane, breaking
                # FHSS coordination. Log at WARN every 5 min and keep
                # polling.
                if (
                    now - self._last_upstream_silent_log_at
                    >= _UPSTREAM_SILENT_LOG_INTERVAL_S
                ):
                    if upstream is not None:
                        log.warning(
                            "wfb_tx_upstream_silent",
                            interface=self._interface,
                            silent_seconds=round(silent_for, 1),
                            rx_queue=upstream[0],
                            drops=upstream[1],
                            note=(
                                "tx_bytes flat but UDP 5600 ingress idle; "
                                "no encoder feeding wfb_tx, skipping kill"
                            ),
                        )
                    else:
                        log.warning(
                            "wfb_tx_upstream_unknown",
                            interface=self._interface,
                            silent_seconds=round(silent_for, 1),
                            note=(
                                "tx_bytes flat and /proc/net/udp row for "
                                "UDP 5600 not found; assuming silent, "
                                "skipping kill"
                            ),
                        )
                    self._last_upstream_silent_log_at = now
                continue

            # Ingress IS feeding (drops advancing or rx_queue non-zero)
            # while tx_bytes stays flat. wfb_tx is wedged — kill and let
            # the main run loop respawn it.
            self._tx_zombie_kills += 1
            log.warning(
                "wfb_tx_zombie_detected",
                interface=self._interface,
                silent_seconds=round(silent_for, 1),
                pid=self._tx_proc.pid if self._tx_proc else None,
                zombie_kills_total=self._tx_zombie_kills,
                upstream_known=upstream is not None,
                note=(
                    "tx_bytes flat while process alive and ingress "
                    "feeding; terminating to trigger restart"
                ),
            )
            try:
                self._tx_proc.terminate()
            except ProcessLookupError:
                pass
            # Reset watchdog state so the next watchdog instance
            # (started after wfb_tx respawns) doesn't immediately
            # fire on the carried-over silence window.
            self._last_tx_byte_value = -1
            self._last_tx_byte_change_at = time.monotonic()
            self._last_upstream_byte_value = -1
            self._last_upstream_change_at = time.monotonic()
            return

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

            # Step 5: control-plane stream (radio_id 1) carrying the
            # FHSS hop coordination frames. ADOS is local-first / field-
            # deployed; the inter-rig link is the WFB radio itself, so
            # HopAnnounce/HopAck must travel here rather than over the
            # operator's LAN where consumer APs frequently drop L2
            # broadcasts wired<->wireless. Failures are non-fatal — the
            # data plane stays up; only FHSS coordination degrades to
            # the legacy LAN-broadcast fallback.
            if tx_ok:
                await self.start_tx_control(interface, self._channel)
                await self.start_rx_control(interface, self._channel)

            if not tx_ok and not rx_ok:
                log.error("wfb_both_failed_to_start")
                self._state = LinkState.DISCONNECTED
                self._restart_count += 1
                # No give-up cap. The drone may have come up before the
                # adapter enumerated; the operator may have unplugged
                # the dongle to wiggle a connector. The agent retries
                # forever — recovery should be automatic the moment
                # the hardware comes back. Fixed 5 s ceiling on the
                # backoff so we don't sit at 30 s after a single bad
                # window when the hardware is fine.
                await asyncio.sleep(backoff)
                backoff = min(backoff * 2, 5.0)
                continue

            backoff = 1.0
            self._state = LinkState.CONNECTED

            # Step 5: Monitor processes and read stats. The TX-liveness
            # watchdog is added alongside _tx_proc.wait() so an alive
            # but silently-zombied wfb_tx still fires the watch loop's
            # FIRST_COMPLETED return and triggers the restart. See
            # operating rule 37 (TX-liveness contract).
            tasks: list[asyncio.Task] = []
            if self._rx_proc is not None:
                tasks.append(asyncio.create_task(self._read_rx_output()))
            if self._tx_proc is not None and self._tx_proc.returncode is None:
                tasks.append(asyncio.create_task(self._tx_proc.wait()))
                tasks.append(asyncio.create_task(self._tx_health_watchdog()))
                tasks.append(asyncio.create_task(self._presence_emit_loop()))
                tasks.append(asyncio.create_task(self._control_plane_listener()))
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

            # No give-up cap on the run loop either. Fixed 5 s ceiling
            # on the backoff (down from 30 s) so we recover snappily
            # when the upstream comes back. Per the GS philosophy: this
            # process exists to relay video — recovery latency dominates
            # over CPU savings.
            await asyncio.sleep(backoff)
            backoff = min(backoff * 2, 5.0)

        self._state = LinkState.DISCONNECTED
