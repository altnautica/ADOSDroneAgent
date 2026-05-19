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
import os
import signal
import sys
import time
from enum import StrEnum
from typing import TYPE_CHECKING

import structlog

from ados.core.config import load_config
from ados.core.logging import configure_logging, get_logger
from ados.core.paths import WFB_KEY_DIR
from ados.services.wfb.adapter import detect_wfb_adapters, set_monitor_mode
from ados.services.wfb.key_mgr import get_key_paths, key_exists
from ados.services.wfb.link_quality import LinkQualityMonitor, LinkStats

# RX-liveness watchdog tunables. Symmetric to wfb/manager.py's TX
# watchdog. We poll rx_packets (not rx_bytes) because the bytes counter
# is unreliable on monitor-mode interfaces with some kernel versions;
# packet count increments per radiotap-framed 802.11 frame regardless.
_RX_HEALTH_POLL_INTERVAL_S = 5.0
_RX_HEALTH_SILENCE_THRESHOLD_S = 30.0

# Phase 11 video pipeline ports.
# wfb_rx outputs FEC-decoded RTP H.264 to UDP _WFB_RX_INTERNAL_UDP_PORT
# (5599). The video fanout reads from there and emits to:
#   - _WFB_RX_MEDIAMTX_UDP_PORT (5600) for the mediamtx-gs ffmpeg
#     ingest sidecar (browser WHEP path, unchanged contract)
#   - _WFB_RX_LCD_UDP_PORT     (5605) for the LCD's udpsrc front end
#     (Phase 11 NEW direct path, bypasses mediamtx-gs RTSP indirection).
#     Moved from 5601 because ffmpeg's -f sdp ingest opens RTCP on
#     RTP+1 by default (RTP=5600, RTCP=5601) and a=rtcp in the SDP
#     is silently ignored by the ffmpeg 5.x build we ship. Putting
#     the LCD tap on 5605 leaves 5601 free for ffmpeg's RTCP.
# Both consumers see every packet; SO_REUSEPORT would load-balance and
# break RTP, so we use an explicit fanout instead.
_WFB_RX_INTERNAL_UDP_PORT = 5599
_WFB_RX_MEDIAMTX_UDP_PORT = 5600
_WFB_RX_LCD_UDP_PORT = 5605

if TYPE_CHECKING:
    from ados.core.config import WfbConfig

log = get_logger("ground_station.wfb_rx")


class LinkState(StrEnum):
    """Ground-side link connection state (mirror of air LinkState)."""

    DISCONNECTED = "disconnected"
    UNPAIRED = "unpaired"
    AUTO_PAIRING = "auto_pairing"
    BINDING = "binding"
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
        # Mirror the air-side preset application so the GS reports the
        # same MCS / FEC values in /api/wfb. The actual radio
        # parameters are set by wfb_tx upstream; the GS-side wfb_rx
        # doesn't read mcs_index, but the operator-visible status
        # block must match.
        from ados.services.wfb.manager import _apply_link_preset

        _apply_link_preset(self._config, log)
        self._state = LinkState.DISCONNECTED
        self._rx_proc: asyncio.subprocess.Process | None = None
        # Phase 11: UDP fan-out subprocess that reads wfb_rx output on
        # the internal port and emits to mediamtx-gs ingest + LCD udpsrc.
        # Spawned alongside wfb_rx and torn down with it.
        self._fanout_proc: asyncio.subprocess.Process | None = None
        # WFB control-plane subprocesses (radio_id 1) that carry FHSS
        # hop coordination over the radio link itself. wfb_rx_control
        # decodes HopAnnounce frames from the drone and emits them on
        # UDP 5803 where HopListener already binds. wfb_tx_control
        # ingests HopAck frames on UDP 5810 from the listener and
        # transmits them back over the radio to the drone supervisor.
        # ADOS is local-first / field-deployed; the inter-rig link
        # must work without depending on a consumer AP bridging L2
        # broadcasts wired<->wireless.
        self._rx_control_proc: asyncio.subprocess.Process | None = None
        self._tx_control_proc: asyncio.subprocess.Process | None = None
        self._monitor = LinkQualityMonitor()
        self._interface: str = ""
        self._channel: int = config.channel
        self._running = False
        self._restart_count = 0
        self._max_restarts = 10
        # RX-liveness watchdog state. We CAN'T use the wlan iface
        # rx_packets counter the way the TX side uses tx_bytes —
        # rx_packets on a monitor-mode interface reflects what the
        # kernel/driver captured from the air, NOT what wfb_rx
        # consumed. SIGSTOP'd wfb_rx still leaves rx_packets
        # incrementing because the kernel keeps capturing 802.11 frames
        # regardless of the userspace consumer.
        # Instead we track when wfb_rx last emitted a stdout stats
        # line — _read_rx_output() updates _last_rx_stdout_at on each
        # successful line. wfb_rx prints stats every second under
        # nominal operation, so 30 s of silence is a strong signal.
        # See operating rule 37.
        self._last_rx_stdout_at: float = 0.0
        self._rx_zombie_kills: int = 0

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
        """Find a WFB-ng compatible adapter that can actually go monitor.

        Returns interface name, or None if no compatible adapter accepts
        the monitor-mode set. Iterates through every compatible adapter
        and tries to set monitor on each — handles the case where the
        agent's chipset-fingerprint scan mislabels a non-RTL adapter
        (e.g., Rock 5C internal AIC8800D80) as WFB-compatible. The
        false-positive will fail `iw set monitor` (different driver
        doesn't support that mode) and we'll fall through to the real
        RTL.

        Thin wrapper over the shared wfb.adapter helper so callers outside
        this module (setup webapp, API, tests) have a single entry point.
        """
        if self._config.interface:
            return self._config.interface
        adapters = detect_wfb_adapters()
        compatible = [a for a in adapters if a.is_wfb_compatible and a.supports_monitor]
        if not compatible:
            return None
        # Try each compatible adapter; first one that accepts monitor-mode
        # set wins. Adapters that fail are skipped, NOT retried, so the
        # outer manager loop's backoff timer doesn't spin on a dead chip.
        for adapter in compatible:
            iface = adapter.interface_name
            log.info(
                "ground_wfb_adapter_candidate",
                interface=iface,
                chipset=adapter.chipset,
            )
            if set_monitor_mode(iface):
                log.info(
                    "ground_wfb_adapter_selected",
                    interface=iface,
                    chipset=adapter.chipset,
                )
                return iface
            log.warning(
                "ground_wfb_adapter_monitor_rejected",
                interface=iface,
                chipset=adapter.chipset,
            )
        log.error("ground_wfb_no_usable_adapter", candidates=len(compatible))
        return None

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
        # `-l 1000` enables wfb_rx's stats line emission once per second.
        # Without it the binary is silent on stdout, _read_rx_output()
        # blocks on readline() forever, LinkQualityMonitor stays empty,
        # and /api/wfb permanently reports state=disabled / rssi=-100 /
        # packets_received=0 even when 802.11 frames are flowing through.
        # `-u 5599`: internal-only port. Phase 11 changed this from the
        # historical 5600 to free that port for the new fan-out's egress.
        # wfb_rx → 5599 → ados-video-fanout → 5600 (mediamtx-gs ingest,
        # unchanged) AND 5601 (LCD udpsrc, NEW direct path that bypasses
        # the mediamtx-gs RTSP indirection). Both consumers see every
        # packet without needing SO_REUSEPORT (which load-balances and
        # would break RTP).
        cmd = [
            "wfb_rx",
            "-p", "0",
            "-c", "127.0.0.1",
            "-u", str(_WFB_RX_INTERNAL_UDP_PORT),
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

    async def start_fanout(self) -> bool:
        """Spawn the UDP fan-out: 5599 → 5600 (mediamtx) + 5601 (LCD).

        The fan-out is a small Python subprocess (sibling to wfb_rx) that
        reads each datagram off the wfb_rx output socket and re-emits it
        to two downstream ports. Both consumers (mediamtx-gs ffmpeg
        ingest on 5600, LocalVideoTap udpsrc on 5601) get every packet.
        """
        cmd = [
            sys.executable,
            "-m",
            "ados.services.ground_station.video_fanout",
            "--listen-host", "127.0.0.1",
            "--listen-port", str(_WFB_RX_INTERNAL_UDP_PORT),
            "--target", f"127.0.0.1:{_WFB_RX_MEDIAMTX_UDP_PORT}",
            "--target", f"127.0.0.1:{_WFB_RX_LCD_UDP_PORT}",
        ]
        try:
            self._fanout_proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.PIPE,
            )
            log.info(
                "ground_video_fanout_started",
                pid=self._fanout_proc.pid,
                listen_port=_WFB_RX_INTERNAL_UDP_PORT,
                mediamtx_port=_WFB_RX_MEDIAMTX_UDP_PORT,
                lcd_port=_WFB_RX_LCD_UDP_PORT,
            )
            return True
        except OSError as exc:
            log.error("ground_video_fanout_start_failed", error=str(exc))
            return False

    async def start_rx_control(self, interface: str, channel: int) -> bool:
        """Spawn wfb_rx_control on radio_id 1 — decodes HopAnnounce.

        The drone's HopSupervisor pushes HopAnnounce frames into its
        wfb_tx_control on radio_id 1; this subprocess decodes them on
        the GS side and emits to UDP 5803 where HopListener already
        binds (see hop_supervisor.HopListener). Same rx.key as the
        data plane: wfb-ng key files carry both halves of the crypto_
        box keypair so a single file authenticates incoming frames
        in both directions.
        """
        _tx_key, rx_key = get_key_paths()
        cmd = [
            "wfb_rx",
            "-p", "1",
            "-c", "127.0.0.1",
            "-u", "5803",
            "-K", rx_key,
            "-l", "1000",
            interface,
        ]
        # stdout=DEVNULL avoids PKT-stats buffer fill. stderr -> log
        # file (truncated on each restart) so wfb-ng diagnostics survive
        # without risking the PIPE deadlock. fd is duped by the kernel
        # for the child; we close our copy after spawn.
        stderr_fd = os.open(
            "/run/ados/wfb-gs-rx-control.log",
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
                "ground_wfb_rx_control_started",
                pid=self._rx_control_proc.pid,
                interface=interface,
                channel=channel,
            )
            return True
        except FileNotFoundError:
            log.error("ground_wfb_rx_control_not_found")
            return False
        except OSError as exc:
            log.error("ground_wfb_rx_control_start_failed", error=str(exc))
            return False
        finally:
            os.close(stderr_fd)

    async def start_tx_control(self, interface: str, channel: int) -> bool:
        """Spawn wfb_tx_control on radio_id 1 — sends HopAck back to drone.

        The GS HopListener writes the HopAck UDP packet to 127.0.0.1:5810;
        wfb_tx_control reads from that port and transmits the frame back
        over the radio so the drone's HopSupervisor (listening on UDP
        5810 served by its own wfb_rx_control) sees the ACK. Same rx.key
        as wfb_rx_control. Lighter FEC than video because control
        frames are tiny.
        """
        _tx_key, rx_key = get_key_paths()
        cmd = [
            "wfb_tx",
            "-p", "1",
            "-u", "5810",
            "-K", rx_key,
            "-k", "1",
            "-n", "2",
            "-B", "20",
            "-M", str(self._config.mcs_index),
            interface,
        ]
        # stdout=DEVNULL; stderr -> log file for wfb-ng diagnostics.
        stderr_fd = os.open(
            "/run/ados/wfb-gs-tx-control.log",
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
                "ground_wfb_tx_control_started",
                pid=self._tx_control_proc.pid,
                interface=interface,
                channel=channel,
            )
            return True
        except FileNotFoundError:
            log.error("ground_wfb_tx_control_not_found")
            return False
        except OSError as exc:
            log.error("ground_wfb_tx_control_start_failed", error=str(exc))
            return False
        finally:
            os.close(stderr_fd)

    async def stop_fanout(self) -> None:
        """Terminate the fan-out subprocess if alive."""
        proc = self._fanout_proc
        if proc is not None and proc.returncode is None:
            try:
                proc.terminate()
                await asyncio.wait_for(proc.wait(), timeout=2.0)
            except TimeoutError:
                try:
                    proc.kill()
                except ProcessLookupError:
                    pass
            except ProcessLookupError:
                pass
        self._fanout_proc = None

    async def stop_rx(self) -> None:
        """Terminate the wfb_rx + control-plane subprocesses if alive."""
        self._running = False
        for name, attr in (
            ("ground_wfb_rx", "_rx_proc"),
            ("ground_wfb_rx_control", "_rx_control_proc"),
            ("ground_wfb_tx_control", "_tx_control_proc"),
        ):
            proc = getattr(self, attr)
            if proc is None or proc.returncode is not None:
                continue
            try:
                proc.terminate()
                await asyncio.wait_for(proc.wait(), timeout=5.0)
                log.info(f"{name}_stopped", pid=proc.pid)
            except TimeoutError:
                try:
                    proc.kill()
                except ProcessLookupError:
                    pass
                try:
                    await asyncio.wait_for(proc.wait(), timeout=1.0)
                except asyncio.TimeoutError:
                    pass
                log.warning(f"{name}_killed", pid=proc.pid)
            except ProcessLookupError:
                log.debug(f"{name}_already_exited")
            setattr(self, attr, None)
        self._state = LinkState.DISCONNECTED

    def stats(self) -> dict:
        """Return the ground-side link stats shape.

        Schema: rssi_dbm, bitrate_mbps, fec_rec, fec_lost, channel,
        plus pair-state fields parity with the air-side status payload
        so heartbeat consumers see the same surface from either side.
        """
        snap: LinkStats = self._monitor.get_current()

        paired = False
        peer_id: str | None = None
        paired_at: str | None = None
        fingerprint: str | None = None
        auto_pair_enabled = bool(
            getattr(self._config, "auto_pair_enabled", False)
        )
        try:
            from ados.services.ground_station.pair_manager import get_pair_manager
            from ados.services.wfb.key_mgr import read_public_fingerprint

            pm = get_pair_manager()
            try:
                fingerprint = read_public_fingerprint(pm.rx_key_path)
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
            # Legacy fall-back: GS profile may carry pair state under
            # ground_station.* on rigs migrating from a pre-0.16 config.
            if peer_id is None:
                gs = getattr(cfg, "ground_station", None) if cfg else None
                if gs is not None:
                    peer_id = getattr(gs, "paired_drone_id", None)
                    if paired_at is None:
                        paired_at = getattr(gs, "paired_at", None)
        except Exception:  # noqa: BLE001
            pass

        return {
            "state": self._state.value,
            "interface": self._interface,
            "channel": self._channel,
            "rssi_dbm": snap.rssi_dbm,
            "bitrate_mbps": round(snap.bitrate_kbps / 1000.0, 3),
            "bitrate_kbps": snap.bitrate_kbps,
            "fec_recovered": snap.fec_recovered,
            "fec_failed": snap.fec_failed,
            "fec_rec": snap.fec_recovered,
            "fec_lost": snap.fec_failed,
            "packets_received": snap.packets_received,
            "packets_lost": snap.packets_lost,
            "loss_percent": snap.loss_percent,
            "snr_db": snap.snr_db,
            "restart_count": self._restart_count,
            "samples": self._monitor.sample_count,
            # Mirror WfbConfig fields onto the same heartbeat shape the
            # air side emits so `build_radio_block` works without a
            # role branch.
            "tx_power_dbm": getattr(self._config, "tx_power_dbm", None),
            "tx_power_max_dbm": getattr(self._config, "tx_power_max_dbm", None),
            "mcs_index": getattr(self._config, "mcs_index", None),
            "topology": getattr(self._config, "topology", None),
            "paired": paired,
            "paired_with_device_id": peer_id,
            "paired_at": paired_at,
            "public_key_fingerprint": fingerprint,
            "auto_pair_enabled": auto_pair_enabled,
            "rx_zombie_kills": self._rx_zombie_kills,
            "rx_silent_seconds": (
                round(time.monotonic() - self._last_rx_stdout_at, 1)
                if self._last_rx_stdout_at > 0
                else None
            ),
        }

    async def _read_rx_output(self) -> None:
        """Feed wfb_rx stdout lines into the link quality monitor.

        Also stamps `_last_rx_stdout_at` on every successful read so
        the RX watchdog can detect a wfb_rx zombie (process alive but
        no longer producing per-second stats lines on stdout).
        """
        if self._rx_proc is None or self._rx_proc.stdout is None:
            return
        from ados.core.paths import WFB_STATS_JSON

        while self._running:
            try:
                line_bytes = await self._rx_proc.stdout.readline()
                if not line_bytes:
                    break
                self._last_rx_stdout_at = time.monotonic()
                line = line_bytes.decode("utf-8", errors="replace").strip()
                if line:
                    snap = self._monitor.feed_line(line)
                    if snap is not None:
                        self._update_state_from_stats(snap)
                        # Mirror the stats to /run/ados/wfb-stats.json
                        # so the API + OLED dashboard tile + Link
                        # Stats LCD page see real values. The api
                        # process can't reach this manager directly
                        # (different systemd unit, different process).
                        self._monitor.persist_to_file(
                            WFB_STATS_JSON,
                            extra={
                                "interface": self._interface,
                                "channel": self._channel,
                                "tx_power_dbm": getattr(
                                    self._config, "tx_power_dbm", None
                                ),
                                "topology": getattr(
                                    self._config, "topology", None
                                ),
                                "mcs_index": getattr(
                                    self._config, "mcs_index", None
                                ),
                                "profile": "ground_station",
                            },
                        )
            except Exception as exc:
                log.debug("ground_rx_read_error", error=str(exc))
                break

    async def _presence_emit_loop(self) -> None:
        """Periodically emit a PresenceBeacon on the WFB control plane.

        Mirror of `WfbManager._presence_emit_loop` with one critical
        asymmetry: on the GS, wfb_tx_control binds UDP 5810 (its
        outbound ingress), while UDP 5803 is wfb_rx_control's output
        AND HopListener's bound port. Sending the beacon to 5803 here
        would loop straight back into HopListener via the kernel
        loopback and self-pair the GS with its own device-id. Send to
        5810 so wfb_tx_control on radio_id 1 transmits the frame over
        RF; the drone's wfb_rx_control decodes and emits on 5810 on
        the drone side for the future drone-side listener to consume.
        """
        try:
            import socket as _socket
            from ados.core.identity import get_or_create_device_id
            from ados.services.wfb.hop_supervisor import (
                PresenceBeacon,
                _PRESENCE_ROLE_GS,
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
        log.info(
            "ground_presence_emit_started",
            device_id=device_id,
            cadence_s=10,
        )
        try:
            while self._running:
                try:
                    pair_key = _resolve_pair_key()
                    beacon = PresenceBeacon(
                        version=_PRESENCE_VERSION_CURRENT,
                        device_id=device_id,
                        role=_PRESENCE_ROLE_GS,
                        channel=int(self._channel or 0),
                        rssi_dbm=0,
                        epoch_ms=int(time.time() * 1000),
                    )
                    payload = beacon.encode(pair_key)
                    sock.sendto(payload, ("127.0.0.1", 5810))
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

    async def _rx_health_watchdog(self) -> None:
        """Catch wfb_rx zombies (process alive but stdout silent).

        wfb_rx prints a stats line on stdout every second under nominal
        operation. _read_rx_output() stamps `_last_rx_stdout_at` on
        each line. If the timestamp hasn't moved for 30 s while the
        process is alive, terminate so the standard restart loop
        respawns it.

        We deliberately do NOT poll /sys/class/net/<iface>/statistics/
        rx_packets — that counter reflects what the kernel/driver
        captured from the air, NOT what wfb_rx consumed. A SIGSTOP'd
        wfb_rx still leaves rx_packets incrementing because the kernel
        keeps capturing 802.11 frames regardless of the userspace
        consumer. Stdout silence is the durable wfb_rx-specific signal.
        See operating rule 37.
        """
        if self._rx_proc is None:
            return
        # Reset the stamp so we don't carry over silence accumulated
        # while the process was being spawned. Give the process a full
        # threshold window to start producing stats.
        self._last_rx_stdout_at = time.monotonic()
        while (
            self._running
            and self._rx_proc is not None
            and self._rx_proc.returncode is None
        ):
            try:
                await asyncio.sleep(_RX_HEALTH_POLL_INTERVAL_S)
            except asyncio.CancelledError:
                return
            silent_for = time.monotonic() - self._last_rx_stdout_at
            if silent_for >= _RX_HEALTH_SILENCE_THRESHOLD_S:
                self._rx_zombie_kills += 1
                log.warning(
                    "ground_wfb_rx_zombie_detected",
                    interface=self._interface,
                    silent_seconds=round(silent_for, 1),
                    pid=self._rx_proc.pid if self._rx_proc else None,
                    zombie_kills_total=self._rx_zombie_kills,
                    note=(
                        "stdout silent while process alive; "
                        "terminating to trigger restart"
                    ),
                )
                try:
                    self._rx_proc.terminate()
                except ProcessLookupError:
                    pass
                self._last_rx_stdout_at = time.monotonic()
                return

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
        unpaired_logged = False

        # Spawn the hop-announce listener as a sibling task. It
        # listens on the control port for valid HopAnnounce
        # packets, ACKs them, and schedules a local channel flip
        # synchronized with the drone's flip epoch. Self-gating:
        # a drone running older code that doesn't broadcast
        # HopAnnounce simply produces no packets here, the
        # listener idles, and the GS stays on the current
        # channel. Disabled when WfbConfig.auto_hop_enabled is
        # false (operator opt-out for fixed-frequency ops).
        if getattr(self._config, "auto_hop_enabled", True):
            try:
                from ados.services.wfb.hop_supervisor import (
                    run_hop_listener,
                )
                self._hop_listener_stop = asyncio.Event()
                self._hop_listener_task = asyncio.create_task(
                    run_hop_listener(
                        wfb_manager=self,
                        band=getattr(self._config, "band", "u-nii-1"),
                        stop_event=self._hop_listener_stop,
                    )
                )
            except Exception as exc:  # noqa: BLE001
                log.warning(
                    "ground_hop_listener_start_failed", error=str(exc)
                )

        while self._running:
            # Block when no GS-side key (rx.key) is on disk. Pairing
            # (local bind, cloud relay, or operator) lands it at
            # WFB_KEY_DIR after a successful bind. WfbRxManager runs
            # on the GS side, so we pass role="gs".
            if not key_exists(role="gs"):
                if not unpaired_logged:
                    log.info(
                        "ground_wfb_blocked_unpaired",
                        expected=f"{WFB_KEY_DIR}/rx.key",
                    )
                    unpaired_logged = True
                self._state = LinkState.UNPAIRED
                await asyncio.sleep(5.0)
                continue
            unpaired_logged = False

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

            # Monitor mode was already set by detect_adapter() above as
            # part of its candidate-iteration. Skipping the duplicate
            # call here avoids re-running iw on an interface that's
            # already in monitor mode.

            # Apply TX power on the monitor interface BEFORE wfb_rx
            # spawns. Same rationale as the air side: without this the
            # dongle runs at driver default (~17-20 dBm) and risks
            # brownout on host-VBUS USB topology.
            from ados.services.wfb.adapter import set_tx_power as _set_tx_power
            effective = _set_tx_power(interface, self._config.tx_power_dbm)
            if effective is None:
                log.warning(
                    "ground_wfb_txpower_not_applied",
                    interface=interface,
                    requested=self._config.tx_power_dbm,
                )

            # Set the radio channel. wfb_rx listens on whatever channel
            # the netdev is set to; it does not change channel itself.
            # Without this, the rig sits on whatever the bind profile
            # or driver default left the radio at.
            try:
                import subprocess as _sp
                ch_result = _sp.run(
                    ["iw", interface, "set", "channel", str(self._channel)],
                    capture_output=True,
                    timeout=5,
                )
                if ch_result.returncode != 0:
                    log.warning(
                        "ground_wfb_channel_set_failed",
                        interface=interface,
                        channel=self._channel,
                        stderr=ch_result.stderr.decode(errors="replace").strip(),
                    )
                else:
                    log.info(
                        "ground_wfb_channel_set",
                        interface=interface,
                        channel=self._channel,
                    )
            except (FileNotFoundError, _sp.TimeoutExpired) as exc:
                log.warning("ground_wfb_channel_set_error", error=str(exc))

            # Key existence already enforced at top of loop. If the key
            # disappeared between then and now (unpair raced with us)
            # the subprocess will exit and we re-enter the loop.

            rx_ok = await self.start_rx(interface, self._channel)
            if not rx_ok:
                log.error("ground_wfb_rx_failed_to_start")
                self._state = LinkState.DISCONNECTED
                self._restart_count += 1
                # Retry forever, snappy ceiling. The drone may not yet
                # have come up; the operator may have unplugged the
                # RTL adapter to wiggle a connector. Recovery should
                # be automatic the moment hardware comes back. The
                # previous 10-restart cap put the rig into a permanent
                # dead state requiring SSH to clear.
                await asyncio.sleep(backoff)
                backoff = min(backoff * 2, 5.0)
                continue

            # Phase 11: spawn the UDP fan-out as soon as wfb_rx is up.
            # The fanout reads wfb_rx output and forwards to both
            # mediamtx-gs and the LCD. Start failure is non-fatal — the
            # browser WHEP path may still come up via mediamtx-gs's
            # direct ingest if some operator override redirects, but
            # the LCD will be silent until the fanout works.
            await self.start_fanout()

            # WFB control-plane subprocesses on radio_id 1 — receive
            # HopAnnounce from the drone and send HopAck back, so FHSS
            # hop coordination travels over the radio link rather than
            # the LAN. Failures are non-fatal — the data plane stays
            # up and the legacy LAN-broadcast HopListener path still
            # works as a fallback while v0.36.25 is in flight.
            await self.start_rx_control(interface, self._channel)
            await self.start_tx_control(interface, self._channel)

            backoff = 1.0
            self._state = LinkState.CONNECTED

            # The RX-liveness watchdog rides alongside _rx_proc.wait() so
            # a process that's alive but no longer producing UDP frames
            # still terminates the wait and triggers the standard restart
            # path. See operating rule 37 (TX-liveness contract).
            tasks: list[asyncio.Task] = []
            if self._rx_proc is not None:
                tasks.append(asyncio.create_task(self._read_rx_output()))
                tasks.append(asyncio.create_task(self._rx_proc.wait()))
                tasks.append(asyncio.create_task(self._rx_health_watchdog()))
                tasks.append(asyncio.create_task(self._presence_emit_loop()))

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

            # Stop fanout BEFORE stop_rx so it doesn't try to forward
            # while wfb_rx is being torn down. Bath both each restart
            # cycle so a stuck fanout never out-lives its rx.
            await self.stop_fanout()
            await self.stop_rx()
            self._running = True

            # No give-up cap. Snappy 5 s ceiling on backoff so recovery
            # is fast when upstream comes back. See operating rule 26.
            await asyncio.sleep(backoff)
            backoff = min(backoff * 2, 5.0)

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

    # NOTE: the auto_pair supervisor is hosted in ados-cloud, not here.
    # Hosting it inside ados-wfb-rx would create a self-kill loop —
    # the bind orchestrator stops ados-wfb-rx to flip wfb-ng profiles,
    # which kills the supervisor running inside it before the bind can
    # finish. ados-cloud is always-running and doesn't own the radio.

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
    await manager.stop_fanout()
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
