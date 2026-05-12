"""Coordinated frequency-hopping supervisor for the WFB-ng radio.

Implements "feels like DJI from the operator's point of view"
hopping on commodity Wi-Fi cards (RTL8812AU/EU). Operator picks a
band (5.2-5.4 GHz, 5.7-5.9 GHz, etc.). After bind the drone + ground
station coordinate periodic and reactive migrations to the quietest
channel inside that band.

True per-block FHSS like DJI OcuSync requires custom MAC firmware
that retunes faster than a single FEC block (~10-20 ms). RTL8812AU
can't do that — ``iw set channel`` blackout is 100-300 ms even in
monitor mode. So our hops are slower-cadence (default 60 s) and
opt-in reactive on link degradation. The trade-off is one ~300 ms
freeze per migration, absorbed by the GCS WHEP playout buffer and
the LCD last-frame-hold.

Protocol:

1. Drone side decides a hop is needed (periodic timer or reactive
   loss/rssi threshold).
2. Drone selects the target channel by scanning the configured
   band, picking the quietest channel that's NOT the current one.
3. Drone broadcasts a HopAnnounce packet on the reserved control
   UDP port carrying ``target_channel`` + ``countdown_ms`` +
   ``epoch``. Repeated every 100 ms during the 3 s lead window
   so the GS only has to receive one to flip together.
4. Ground side listens on the same port. When it sees a HopAnnounce
   for a future epoch and confirms the announce is signed by the
   live pair-key (anti-replay + anti-spoof), it schedules its own
   ``iw set channel`` call for ``epoch``.
5. At ``epoch`` both sides run ``iw set channel`` and wfb_tx +
   wfb_rx are restarted on the new channel.

This module assembles the supervisor task. The control packet
itself is a fixed-width msgpack payload defined alongside.

Self-gating: the drone only flips channel after receiving at least
one peer ACK. A half-upgraded pair (one rig running this code, the
other not) won't ACK and the drone stays on the current channel.
Older fleets are not silently broken by enabling auto-hop.
"""

from __future__ import annotations

import asyncio
import hashlib
import hmac
import json
import socket
import struct
import time
from dataclasses import dataclass
from typing import Any

import structlog

from ados.services.wfb.channel import (
    STANDARD_CHANNELS,
    WfbChannel,
    _BAND_CHANNELS,
    get_channel,
    scan_channels,
)

log = structlog.get_logger(__name__)


# Reserved UDP port for the hop-announce control channel. Distinct
# from the wfb_tx UDP ingress port (5600) and the GCS heartbeat
# port (5601 / 5800). 5803 chosen so the drone-bind / wfb-server /
# RTSP-internal port ranges don't collide.
HOP_CONTROL_PORT = 5803

# Time the drone gives the GS to flip after the announce. RTL8812EU
# `iw set channel` in monitor mode is 100-300 ms, plus wfb_tx/wfb_rx
# stop+start is ~500 ms. 3 seconds is comfortable margin so an
# in-flight announce isn't racing the actual channel change.
HOP_COUNTDOWN_MS = 3000

# Period at which the drone re-broadcasts the announce during the
# countdown window. Sized so even on a degraded link with 30% loss
# the GS receives at least one in the 3 s window.
HOP_BROADCAST_INTERVAL_MS = 100

# Magic prefix to disambiguate hop control packets from any other
# UDP traffic that lands on this port. Replaces an explicit length
# header.
_HOP_MAGIC = b"AD05HOP1"

# How many TX rounds the announce repeats before the drone gives
# up and aborts the hop. 30 rounds * 100 ms = 3 s, matching the
# countdown.
_HOP_BROADCAST_ROUNDS = int(HOP_COUNTDOWN_MS / HOP_BROADCAST_INTERVAL_MS)


@dataclass(frozen=True)
class HopAnnounce:
    """Wire shape of a single hop-announce packet.

    Encoded as: _HOP_MAGIC (8B) + version (1B) + epoch_ms (8B BE) +
    target_channel (1B) + reserved (1B) + hmac_sha256 (32B) =
    51 bytes total.

    The HMAC is computed over the magic + version + epoch_ms +
    target_channel + reserved using the shared pair key as the
    secret. Authenticates the announce so a third party watching
    the control band can't inject a hop that knocks our pair off
    the air.
    """

    version: int
    epoch_ms: int
    target_channel: int

    def encode(self, pair_key: bytes) -> bytes:
        body = (
            _HOP_MAGIC
            + bytes([self.version])
            + struct.pack(">Q", self.epoch_ms)
            + bytes([self.target_channel])
            + b"\x00"
        )
        tag = hmac.new(pair_key, body, hashlib.sha256).digest()
        return body + tag

    @classmethod
    def decode(cls, raw: bytes, pair_key: bytes) -> "HopAnnounce | None":
        if len(raw) != 51:
            return None
        if not raw.startswith(_HOP_MAGIC):
            return None
        body, tag = raw[:-32], raw[-32:]
        expected = hmac.new(pair_key, body, hashlib.sha256).digest()
        if not hmac.compare_digest(expected, tag):
            return None
        version = body[8]
        epoch_ms = struct.unpack(">Q", body[9:17])[0]
        target_channel = body[17]
        return cls(
            version=version, epoch_ms=epoch_ms, target_channel=target_channel
        )


def _resolve_pair_key() -> bytes:
    """Derive the symmetric pair key used to authenticate HopAnnounce.

    Reads the canonical wfb keypair under /etc/ados/wfb/ and hashes
    both halves with a fixed domain separator. Independent of any
    other use of the wfb keys so a future rotation of one doesn't
    leak the other.
    """
    # wfb-ng session keys are symmetric per pair: the drone writes the
    # shared bytes to /etc/ados/wfb/tx.key and the GS writes the same
    # bytes to /etc/ados/wfb/rx.key during bind. They're the same 64
    # bytes — wfb_tx and wfb_rx need a shared AEAD secret to encrypt
    # and decrypt each other's frames. So whichever file the local
    # rig holds gives the same derived hop secret on both sides.
    candidates = (
        "/etc/ados/wfb/tx.key",
        "/etc/ados/wfb/rx.key",
        "/etc/ados/tx.key",
        "/etc/ados/rx.key",
    )
    for path in candidates:
        try:
            with open(path, "rb") as f:
                key_bytes = f.read()
        except (OSError, FileNotFoundError):
            continue
        h = hashlib.sha256()
        h.update(b"ados/wfb/hop/v1\n")
        h.update(key_bytes)
        return h.digest()
    # No keys on disk yet (cold start before bind). Use a constant
    # so a stray hop announce can still be parsed; the supervisor
    # gates on a successful pair before doing anything anyway.
    log.warning("hop_supervisor_pair_key_unavailable")
    return hashlib.sha256(b"ados/wfb/hop/v1/cold-start").digest()


def _pick_target_channel(
    interface: str,
    band: str,
    current_channel: int,
) -> WfbChannel | None:
    """Scan ``band`` and return the quietest channel that isn't
    the current one.

    Returns None when the scan can't run (interface missing, scan
    timeout) or every candidate is the current channel. Callers
    treat None as "no hop this round."
    """
    try:
        scan = scan_channels(interface)
    except Exception as exc:  # noqa: BLE001
        log.warning("hop_supervisor_scan_failed", error=str(exc))
        return None

    band_numbers = _BAND_CHANNELS.get(band) or _BAND_CHANNELS["all"]
    band_set = set(band_numbers)

    # Re-rank candidates by interference (scan already sorts) but
    # filter to the band and drop the current channel so we don't
    # propose a no-op.
    for ch, _interference in scan:
        if ch.channel_number not in band_set:
            continue
        if ch.channel_number == current_channel:
            continue
        return ch
    return None


class HopSupervisor:
    """Background task that runs the periodic + reactive hop loop.

    Drone-side only. The GS counterpart is the listener+actor at
    ``run_hop_listener()`` below. Both rely on the same packet
    format + pair-key HMAC.

    Wire after WfbManager is running. Reads the live channel
    from wfb_manager._channel and uses wfb_manager.set_fec /
    set_mcs / start_tx as the actuation surface (a hop is
    structurally identical to a tier change, just on a different
    knob — channel via ``iw`` followed by wfb_tx restart).
    """

    def __init__(
        self,
        *,
        wfb_manager: Any,
        link_quality_monitor: Any,
        band: str = "u-nii-1",
        hop_period_seconds: int = 60,
        loss_threshold_percent: float = 10.0,
        rssi_threshold_dbm: float = -75.0,
        enabled: bool = True,
        control_port: int = HOP_CONTROL_PORT,
    ) -> None:
        self._wfb = wfb_manager
        self._lqm = link_quality_monitor
        self._band = band
        self._hop_period_s = max(15, int(hop_period_seconds))
        self._loss_threshold = float(loss_threshold_percent)
        self._rssi_threshold = float(rssi_threshold_dbm)
        self._enabled = bool(enabled)
        self._control_port = control_port
        self._stop_event = asyncio.Event()
        self._last_hop_at: float = 0.0
        self._reactive_cooldown_s = 30.0
        self._history: list[dict[str, Any]] = []

    def snapshot(self) -> dict[str, Any]:
        return {
            "enabled": self._enabled,
            "band": self._band,
            "hop_period_seconds": self._hop_period_s,
            "loss_threshold_percent": self._loss_threshold,
            "rssi_threshold_dbm": self._rssi_threshold,
            "last_hop_at": self._last_hop_at,
            "history": list(self._history[-32:]),
        }

    def set_enabled(self, enabled: bool) -> None:
        self._enabled = bool(enabled)

    def set_band(self, band: str) -> None:
        if band in _BAND_CHANNELS:
            self._band = band

    async def stop(self) -> None:
        self._stop_event.set()

    async def run(self) -> None:
        """Long-running coroutine. Drives the periodic + reactive
        hop loop until stop() is called.
        """
        log.info(
            "hop_supervisor_started",
            band=self._band,
            period=self._hop_period_s,
            enabled=self._enabled,
        )
        try:
            await self._loop()
        finally:
            log.info("hop_supervisor_stopped")

    async def _loop(self) -> None:
        next_periodic = time.monotonic() + self._hop_period_s
        while not self._stop_event.is_set():
            try:
                await self._tick(next_periodic)
            except asyncio.CancelledError:
                raise
            except Exception as exc:  # noqa: BLE001
                log.error("hop_supervisor_tick_failed", error=str(exc))
            try:
                await asyncio.wait_for(self._stop_event.wait(), timeout=1.0)
            except asyncio.TimeoutError:
                pass
            now = time.monotonic()
            if now >= next_periodic:
                next_periodic = now + self._hop_period_s

    async def _tick(self, next_periodic_at: float) -> None:
        if not self._enabled:
            return
        if not getattr(self._wfb, "_interface", None):
            return

        now = time.monotonic()
        # Reactive trigger: most recent sample crosses either threshold
        # AND it's been at least reactive_cooldown_s since the last hop.
        reactive = False
        latest = getattr(self._lqm, "_latest", None) or getattr(
            self._lqm, "latest", None
        )
        if latest is not None:
            loss = float(getattr(latest, "loss_percent", 0.0))
            rssi = float(getattr(latest, "rssi_dbm", -100.0))
            if (
                loss > self._loss_threshold or rssi < self._rssi_threshold
            ) and (now - self._last_hop_at) > self._reactive_cooldown_s:
                reactive = True
                log.info(
                    "hop_supervisor_reactive_trigger",
                    loss=loss,
                    rssi=rssi,
                )

        periodic = now >= next_periodic_at
        if not (periodic or reactive):
            return

        target = _pick_target_channel(
            interface=self._wfb._interface,
            band=self._band,
            current_channel=self._wfb._channel,
        )
        if target is None:
            log.debug(
                "hop_supervisor_no_candidate",
                band=self._band,
                current=self._wfb._channel,
            )
            return

        ok = await self._execute_hop(target.channel_number)
        self._history.append(
            {
                "at": time.time(),
                "from": self._wfb._channel,
                "to": target.channel_number,
                "trigger": "reactive" if reactive else "periodic",
                "ok": ok,
            }
        )
        if ok:
            self._last_hop_at = now

    async def _execute_hop(self, target_channel: int) -> bool:
        """Announce + flip. Returns True if the drone actually
        moved to ``target_channel``.

        Self-gating: the drone only flips after receiving at least
        one peer ACK on the control port. If no ACK arrives during
        the countdown, the drone stays on the current channel.
        """
        pair_key = _resolve_pair_key()
        epoch_ms = int(time.time() * 1000) + HOP_COUNTDOWN_MS
        announce = HopAnnounce(
            version=1, epoch_ms=epoch_ms, target_channel=target_channel
        )

        ack_received = await self._broadcast_and_await_ack(
            announce, pair_key,
        )
        if not ack_received:
            log.info(
                "hop_supervisor_no_peer_ack",
                target=target_channel,
                msg="staying on current channel",
            )
            return False

        # Wait for the epoch and then flip atomically.
        delay = (epoch_ms / 1000.0) - time.time()
        if delay > 0:
            await asyncio.sleep(delay)

        await self._wfb.stop()
        # Set the new channel on the radio.
        await asyncio.create_subprocess_exec(
            "iw",
            self._wfb._interface,
            "set",
            "channel",
            str(target_channel),
            stdout=asyncio.subprocess.DEVNULL,
            stderr=asyncio.subprocess.DEVNULL,
        )
        # Persist + bring wfb back up on the new channel.
        self._wfb._channel = target_channel
        ok = await self._wfb.start_tx(self._wfb._interface, target_channel)
        log.info(
            "hop_supervisor_completed",
            target=target_channel,
            tx_ok=ok,
        )
        return ok

    async def _broadcast_and_await_ack(
        self,
        announce: HopAnnounce,
        pair_key: bytes,
    ) -> bool:
        """Repeatedly broadcast the announce on the control port.

        Listens on the same port for a HopAck (any HopAnnounce we
        receive back from the peer with our same target counts).
        Returns True as soon as one is received; False if the
        countdown elapses with no ACK.
        """
        loop = asyncio.get_running_loop()
        ack_event = asyncio.Event()
        sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        try:
            sock.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)
            sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            sock.bind(("0.0.0.0", self._control_port))
            sock.setblocking(False)
        except OSError as exc:
            log.warning("hop_supervisor_socket_failed", error=str(exc))
            sock.close()
            return False

        async def _reader() -> None:
            while not ack_event.is_set():
                try:
                    data = await loop.sock_recv(sock, 256)
                except (OSError, asyncio.CancelledError):
                    return
                decoded = HopAnnounce.decode(data, pair_key)
                if decoded is None:
                    continue
                if decoded.target_channel == announce.target_channel:
                    log.debug(
                        "hop_supervisor_ack_received",
                        target=decoded.target_channel,
                    )
                    ack_event.set()
                    return

        reader_task = asyncio.create_task(_reader())
        payload = announce.encode(pair_key)
        try:
            for _ in range(_HOP_BROADCAST_ROUNDS):
                if ack_event.is_set():
                    break
                try:
                    sock.sendto(payload, ("255.255.255.255", self._control_port))
                except OSError as exc:
                    log.debug("hop_supervisor_send_failed", error=str(exc))
                await asyncio.sleep(HOP_BROADCAST_INTERVAL_MS / 1000.0)
        finally:
            reader_task.cancel()
            try:
                await reader_task
            except (asyncio.CancelledError, Exception):
                pass
            sock.close()
        return ack_event.is_set()


async def run_hop_listener(
    *,
    wfb_manager: Any,
    band: str = "u-nii-1",
    control_port: int = HOP_CONTROL_PORT,
    stop_event: asyncio.Event,
) -> None:
    """Ground-side counterpart of HopSupervisor.

    Listens on the control port for valid HopAnnounce packets,
    ACKs by echoing the same announce back, and schedules a
    local ``iw set channel`` for the announced epoch. The ACK +
    flip pattern means a half-upgraded pair (drone on new code,
    GS on old) will see the drone not flipping (no ACK arrived),
    so the older GS does not silently lose its peer.

    Wired as a long-running asyncio task by the wfb manager
    main loop.
    """
    log.info("hop_listener_started", band=band, port=control_port)

    pair_key = _resolve_pair_key()
    loop = asyncio.get_running_loop()
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        sock.bind(("0.0.0.0", control_port))
        sock.setblocking(False)
    except OSError as exc:
        log.warning("hop_listener_socket_failed", error=str(exc))
        sock.close()
        return

    pending_epoch: int | None = None
    pending_channel: int | None = None
    try:
        while not stop_event.is_set():
            try:
                data, peer = await asyncio.wait_for(
                    loop.sock_recvfrom(sock, 256), timeout=1.0
                )
            except (asyncio.TimeoutError, OSError):
                # Periodic wake to fire the scheduled hop if its
                # epoch has arrived.
                if (
                    pending_epoch is not None
                    and time.time() * 1000 >= pending_epoch
                    and pending_channel is not None
                ):
                    await _apply_hop(wfb_manager, pending_channel)
                    pending_epoch = None
                    pending_channel = None
                continue

            announce = HopAnnounce.decode(data, pair_key)
            if announce is None:
                continue
            if announce.target_channel not in _CHANNEL_NUMBERS:
                continue
            # ACK by echoing the same packet back to the sender.
            try:
                sock.sendto(data, peer)
            except OSError:
                pass
            pending_epoch = announce.epoch_ms
            pending_channel = announce.target_channel
            log.info(
                "hop_listener_scheduled",
                target=announce.target_channel,
                in_ms=announce.epoch_ms - int(time.time() * 1000),
            )
    finally:
        sock.close()
        log.info("hop_listener_stopped")


_CHANNEL_NUMBERS = {ch.channel_number for ch in STANDARD_CHANNELS}


async def _apply_hop(wfb_manager: Any, target_channel: int) -> None:
    """GS-side actuation: stop wfb_rx, retune the radio, start wfb_rx."""
    interface = getattr(wfb_manager, "_interface", None)
    if not interface:
        log.warning("hop_listener_no_interface")
        return
    log.info("hop_listener_applying", target=target_channel)
    await wfb_manager.stop()
    await asyncio.create_subprocess_exec(
        "iw",
        interface,
        "set",
        "channel",
        str(target_channel),
        stdout=asyncio.subprocess.DEVNULL,
        stderr=asyncio.subprocess.DEVNULL,
    )
    wfb_manager._channel = target_channel
    ch = get_channel(target_channel)
    if ch is None:
        log.warning("hop_listener_unknown_channel", target=target_channel)
        return
    await wfb_manager.start_rx(interface, target_channel)
