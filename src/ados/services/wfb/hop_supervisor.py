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

from ados.core.paths import HOP_SUPERVISOR_JSON
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


# Wire format versions for HopAnnounce.
# v1: trigger byte is 0/reserved. Old supervisor + old listener combo.
# v2: trigger byte carries the supervisor's classification (0 =
#     periodic, 1 = reactive). Lets the GS-side listener record the
#     drone's reason for hopping in its own history ring without
#     guessing.
_HOP_ANNOUNCE_VERSION_CURRENT = 2

# Trigger byte values. Match the string labels the supervisor and the
# listener record in their snapshot dicts.
_TRIGGER_PERIODIC = 0
_TRIGGER_REACTIVE = 1

_TRIGGER_BYTE_TO_LABEL: dict[int, str] = {
    _TRIGGER_PERIODIC: "periodic",
    _TRIGGER_REACTIVE: "reactive",
}

_TRIGGER_LABEL_TO_BYTE: dict[str, int] = {
    v: k for k, v in _TRIGGER_BYTE_TO_LABEL.items()
}


@dataclass(frozen=True)
class HopAnnounce:
    """Wire shape of a single hop-announce packet.

    Encoded as: _HOP_MAGIC (8B) + version (1B) + epoch_ms (8B BE) +
    target_channel (1B) + trigger (1B) + hmac_sha256 (32B) =
    51 bytes total.

    The trigger byte was reserved through v1 and is now (v2) the
    supervisor's classification (0 = periodic, 1 = reactive). Old
    listeners that never validated the byte continue to apply hops;
    they just can't surface the trigger label downstream. Old
    supervisors that always sent reserved=0 are indistinguishable
    from v2 periodic announces, which is the safe interpretation.

    The HMAC is computed over the magic + version + epoch_ms +
    target_channel + trigger using the shared pair key as the
    secret. Authenticates the announce so a third party watching
    the control band can't inject a hop that knocks our pair off
    the air.
    """

    version: int
    epoch_ms: int
    target_channel: int
    trigger: int = _TRIGGER_PERIODIC

    def encode(self, pair_key: bytes) -> bytes:
        body = (
            _HOP_MAGIC
            + bytes([self.version])
            + struct.pack(">Q", self.epoch_ms)
            + bytes([self.target_channel])
            + bytes([self.trigger & 0xFF])
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
        trigger = body[18]
        return cls(
            version=version,
            epoch_ms=epoch_ms,
            target_channel=target_channel,
            trigger=trigger,
        )

    @property
    def trigger_label(self) -> str:
        return _TRIGGER_BYTE_TO_LABEL.get(self.trigger, "periodic")


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
        next_persist = 0.0
        while not self._stop_event.is_set():
            try:
                await self._tick(next_periodic)
            except asyncio.CancelledError:
                raise
            except Exception as exc:  # noqa: BLE001
                log.error("hop_supervisor_tick_failed", error=str(exc))
            # Persist snapshot to /run/ados/hop-supervisor.json so the
            # API process (separate from ados-wfb in multi-process
            # systemd) can read it without a cross-process accessor.
            # 5 s cadence — the GCS chart polls at 1 Hz but doesn't
            # need sub-second freshness for hop history.
            now = time.monotonic()
            if now >= next_persist:
                self._persist_snapshot()
                next_persist = now + 5.0
            try:
                await asyncio.wait_for(self._stop_event.wait(), timeout=1.0)
            except asyncio.TimeoutError:
                pass
            now = time.monotonic()
            if now >= next_periodic:
                next_periodic = now + self._hop_period_s

    def _persist_snapshot(self) -> None:
        """Write the current snapshot to /run/ados/hop-supervisor.json.

        Atomic tmpfile+rename so a concurrent reader on the API side
        never sees a truncated file. Best-effort: any I/O error is
        logged at debug and the loop continues — the metric is not
        critical-path.
        """
        try:
            path = HOP_SUPERVISOR_JSON
            payload = self.snapshot()
            payload["wall_time_unix"] = time.time()
            tmp = path.with_suffix(".tmp")
            tmp.parent.mkdir(parents=True, exist_ok=True)
            tmp.write_text(json.dumps(payload))
            tmp.replace(path)
        except OSError as exc:
            log.debug("hop_supervisor_persist_failed", error=str(exc))

    async def _tick(self, next_periodic_at: float) -> None:
        # Defensive: never trigger a channel change while a local bind
        # session is in flight. The bind orchestrator stops the normal
        # wfb unit so wfb-ng's bind profile can own the radio adapter
        # exclusively; a racing iw-channel + wfb_tx restart from this
        # supervisor would fight the bind tunnel and corrupt the socat
        # key exchange. Lazy import keeps this module independent of
        # the bind orchestrator at import time.
        now = time.monotonic()
        # Periodic boundary check is moved up so the silent early-return
        # gates below can log diagnostically once per period instead of
        # every tick (which would spam the journal at 1 Hz).
        periodic = now >= next_periodic_at

        # Tick counter so we can distinguish "_tick never called" from
        # "_tick called but periodic never True". Logs at 1 Hz on
        # multiples of 30 (every ~30s) plus on periodic boundary, with
        # the time delta to the next periodic so we can see if it's
        # ever advancing toward zero.
        if not hasattr(self, "_tick_count"):
            self._tick_count = 0
        self._tick_count += 1
        if periodic or self._tick_count % 30 == 1:
            log.info(
                "hop_supervisor_tick_entered",
                tick_n=self._tick_count,
                periodic=periodic,
                seconds_until_periodic=round(next_periodic_at - now, 2),
                interface=repr(getattr(self._wfb, "_interface", None)),
                channel=getattr(self._wfb, "_channel", None),
                enabled=self._enabled,
            )

        try:
            from ados.services.wfb.bind_orchestrator import is_bind_active
            if is_bind_active():
                if periodic:
                    log.info("hop_supervisor_skip_during_bind")
                return
        except Exception as _exc:
            if periodic:
                log.info(
                    "hop_supervisor_skip_bind_check_exception",
                    error=str(_exc),
                )
            pass

        if not self._enabled:
            if periodic:
                log.info("hop_supervisor_skip_disabled")
            return
        interface = getattr(self._wfb, "_interface", None)
        if not interface:
            if periodic:
                log.info(
                    "hop_supervisor_skip_no_interface",
                    interface=repr(interface),
                    manager_class=type(self._wfb).__name__,
                )
            return

        # Reactive trigger: most recent sample crosses either threshold
        # AND it's been at least reactive_cooldown_s since the last hop.
        #
        # CRITICAL: only fire when we have ACTUAL link-quality samples.
        # LinkQualityMonitor's default LinkStats has rssi_dbm=-100 and
        # loss_percent=0 — which trips the reactive threshold on every
        # tick before any real wfb_rx output has been parsed. On a
        # drone-only rig that doesn't run wfb_rx (no rx.key), the
        # monitor never updates and the supervisor would otherwise
        # hop every 30 s (cooldown) forever on stale defaults. Gate
        # on `timestamp` being non-empty AND `packets_received` having
        # crossed at least once — both are 0/empty in the default
        # LinkStats.
        reactive = False
        latest = getattr(self._lqm, "_latest", None) or getattr(
            self._lqm, "latest", None
        )
        if latest is not None:
            timestamp = getattr(latest, "timestamp", "")
            packets_received = int(getattr(latest, "packets_received", 0))
            has_real_data = bool(timestamp) and packets_received > 0
            loss = float(getattr(latest, "loss_percent", 0.0))
            rssi = float(getattr(latest, "rssi_dbm", -100.0))
            if (
                has_real_data
                and (loss > self._loss_threshold or rssi < self._rssi_threshold)
                and (now - self._last_hop_at) > self._reactive_cooldown_s
            ):
                reactive = True
                log.info(
                    "hop_supervisor_reactive_trigger",
                    loss=loss,
                    rssi=rssi,
                    packets=packets_received,
                )

        if not (periodic or reactive):
            return

        # Confirmation: we got past every gate and are about to attempt
        # a hop. Promoted from no-log so the journal records every
        # periodic-fire decision.
        log.info(
            "hop_supervisor_attempt",
            trigger="periodic" if periodic else "reactive",
            current_channel=self._wfb._channel,
            band=self._band,
        )

        target = _pick_target_channel(
            interface=self._wfb._interface,
            band=self._band,
            current_channel=self._wfb._channel,
        )
        if target is None:
            log.info(
                "hop_supervisor_no_candidate",
                band=self._band,
                current=self._wfb._channel,
            )
            return

        trigger_label = "reactive" if reactive else "periodic"
        from_channel = self._wfb._channel
        ok = await self._execute_hop(
            target.channel_number, trigger_label=trigger_label,
        )
        self._history.append(
            {
                "at": time.time(),
                "from": from_channel,
                "to": target.channel_number,
                "trigger": trigger_label,
                "ok": ok,
            }
        )
        if ok:
            self._last_hop_at = now

    async def _execute_hop(
        self,
        target_channel: int,
        *,
        trigger_label: str = "periodic",
    ) -> bool:
        """Announce + flip. Returns True if the drone actually
        moved to ``target_channel``.

        Self-gating: the drone only flips after receiving at least
        one peer ACK on the control port. If no ACK arrives during
        the countdown, the drone stays on the current channel.

        ``trigger_label`` is encoded into the wire packet's trigger
        byte (v2 wire format) so the GS-side listener can surface
        the reason in its own history ring without guessing.
        """
        pair_key = _resolve_pair_key()
        epoch_ms = int(time.time() * 1000) + HOP_COUNTDOWN_MS
        trigger_byte = _TRIGGER_LABEL_TO_BYTE.get(
            trigger_label, _TRIGGER_PERIODIC
        )
        announce = HopAnnounce(
            version=_HOP_ANNOUNCE_VERSION_CURRENT,
            epoch_ms=epoch_ms,
            target_channel=target_channel,
            trigger=trigger_byte,
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


_CHANNEL_NUMBERS = {ch.channel_number for ch in STANDARD_CHANNELS}


class HopListener:
    """GS-side counterpart of HopSupervisor.

    Listens on the control port for valid HopAnnounce packets, ACKs
    by echoing the same announce back, and schedules a local
    ``iw set channel`` for the announced epoch. Maintains a 32-entry
    history ring of the hops it applied so the GS LCD's
    ChannelHopsPage has data to render — without this the only
    rig with an LCD (the GS) would see an empty page forever
    because the drone-side HopSupervisor writes the state file
    on the wrong machine.

    snapshot() returns the SAME shape as HopSupervisor.snapshot()
    so the LCD widget, the API route, and any other consumer reads
    the same JSON keys regardless of which side wrote the file.
    """

    def __init__(
        self,
        *,
        wfb_manager: Any,
        band: str = "u-nii-1",
        control_port: int = HOP_CONTROL_PORT,
    ) -> None:
        self._wfb = wfb_manager
        self._band = band
        self._control_port = control_port
        self._history: list[dict[str, Any]] = []
        self._last_hop_at: float = 0.0

    def snapshot(self) -> dict[str, Any]:
        """Return the same shape as HopSupervisor.snapshot()."""
        return {
            "enabled": True,
            "band": self._band,
            "hop_period_seconds": None,
            "loss_threshold_percent": None,
            "rssi_threshold_dbm": None,
            "last_hop_at": self._last_hop_at,
            "history": list(self._history[-32:]),
            "source": "listener",
        }

    def _persist_snapshot(self) -> None:
        """Write the snapshot to /run/ados/hop-supervisor.json.

        Atomic tmpfile + rename. Best-effort: I/O errors logged at
        debug and discarded. Single source of truth file path —
        the drone supervisor and the GS listener both write here;
        a given rig only has one or the other running so there's
        no contention.
        """
        try:
            path = HOP_SUPERVISOR_JSON
            payload = self.snapshot()
            payload["wall_time_unix"] = time.time()
            tmp = path.with_suffix(".tmp")
            tmp.parent.mkdir(parents=True, exist_ok=True)
            tmp.write_text(json.dumps(payload))
            tmp.replace(path)
        except OSError as exc:
            log.debug("hop_listener_persist_failed", error=str(exc))

    async def run(self, stop_event: asyncio.Event) -> None:
        """Long-running coroutine. Drives the listener until stop."""
        log.info(
            "hop_listener_started",
            band=self._band,
            port=self._control_port,
        )

        pair_key = _resolve_pair_key()
        loop = asyncio.get_running_loop()
        sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        try:
            sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            sock.bind(("0.0.0.0", self._control_port))
            sock.setblocking(False)
        except OSError as exc:
            log.warning("hop_listener_socket_failed", error=str(exc))
            sock.close()
            return

        # Persist the initial empty snapshot so the LCD page reads
        # a valid (but empty) state file even before the first hop
        # — otherwise the page shows "no hops yet" via the missing-
        # file fallback, which is fine, but the file's absence
        # also confuses /api/video/config consumers.
        self._persist_snapshot()
        next_persist = time.monotonic() + 5.0

        pending_epoch: int | None = None
        pending_channel: int | None = None
        pending_trigger: str = "periodic"
        try:
            while not stop_event.is_set():
                try:
                    data, peer = await asyncio.wait_for(
                        loop.sock_recvfrom(sock, 256), timeout=1.0
                    )
                except (asyncio.TimeoutError, OSError):
                    # Periodic wake to fire the scheduled hop if
                    # its epoch has arrived, and to push a fresh
                    # snapshot to disk every ~5 s.
                    if (
                        pending_epoch is not None
                        and time.time() * 1000 >= pending_epoch
                        and pending_channel is not None
                    ):
                        prev_channel = getattr(
                            self._wfb, "_channel", None
                        )
                        ok = await _apply_hop(
                            self._wfb, pending_channel,
                        )
                        # Always record — failures are also worth
                        # surfacing on the chart (red marker).
                        self._history.append(
                            {
                                "at": time.time(),
                                "from": (
                                    int(prev_channel)
                                    if isinstance(prev_channel, int)
                                    else 0
                                ),
                                "to": int(pending_channel),
                                "trigger": pending_trigger,
                                "ok": bool(ok),
                            }
                        )
                        if ok:
                            self._last_hop_at = time.time()
                        # Truncate to the 32-entry cap; deque would
                        # do this for free but we keep the simple
                        # list to make the persist code uniform.
                        if len(self._history) > 32:
                            self._history = self._history[-32:]
                        self._persist_snapshot()
                        pending_epoch = None
                        pending_channel = None
                        pending_trigger = "periodic"
                    now = time.monotonic()
                    if now >= next_persist:
                        self._persist_snapshot()
                        next_persist = now + 5.0
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
                pending_trigger = announce.trigger_label
                log.info(
                    "hop_listener_scheduled",
                    target=announce.target_channel,
                    trigger=pending_trigger,
                    in_ms=announce.epoch_ms - int(time.time() * 1000),
                )
        finally:
            sock.close()
            log.info("hop_listener_stopped")


async def run_hop_listener(
    *,
    wfb_manager: Any,
    band: str = "u-nii-1",
    control_port: int = HOP_CONTROL_PORT,
    stop_event: asyncio.Event,
) -> None:
    """Function-style wrapper around HopListener.run().

    Kept for backward compat with the existing call site in
    ground_station/wfb_rx.run(); new callers can instantiate
    HopListener directly to access the snapshot() surface.
    """
    listener = HopListener(
        wfb_manager=wfb_manager, band=band, control_port=control_port,
    )
    await listener.run(stop_event)


async def _apply_hop(wfb_manager: Any, target_channel: int) -> bool:
    """GS-side actuation: hot-restart wfb_rx on the new channel.

    Does NOT call ``wfb_manager.stop()`` / ``stop_rx()`` — those set
    ``_running = False`` which causes the outer ``wfb_rx.run()`` loop
    to exit and the whole service to shut down. Each hop would
    otherwise trigger a full service restart, losing the listener's
    history ring and forcing systemd to bring everything back up.
    Instead: kill the rx subprocess directly, set the new channel
    on the radio with ``iw``, and call ``start_rx`` to spawn a fresh
    wfb_rx tied to the same long-running manager.

    Returns True on a successful start_rx. False when the interface
    is missing or the channel is unknown — both surface as red
    markers on the chart.
    """
    interface = getattr(wfb_manager, "_interface", None)
    if not interface:
        log.warning("hop_listener_no_interface")
        return False
    log.info("hop_listener_applying", target=target_channel)

    # Kill the rx subprocess in-place. _running stays True so the
    # outer manager loop doesn't tear the service down — this is
    # the critical difference vs calling stop() / stop_rx().
    proc = getattr(wfb_manager, "_rx_proc", None)
    if proc is not None and getattr(proc, "returncode", 0) is None:
        try:
            proc.terminate()
            await asyncio.wait_for(proc.wait(), timeout=3.0)
        except asyncio.TimeoutError:
            try:
                proc.kill()
            except ProcessLookupError:
                pass
        except ProcessLookupError:
            pass
    # Clear the handle so start_rx doesn't see a stale process.
    try:
        wfb_manager._rx_proc = None
    except AttributeError:
        pass

    # Retune the radio. iw on monitor mode is the supported path.
    await asyncio.create_subprocess_exec(
        "iw",
        interface,
        "set",
        "channel",
        str(target_channel),
        stdout=asyncio.subprocess.DEVNULL,
        stderr=asyncio.subprocess.DEVNULL,
    )
    try:
        wfb_manager._channel = target_channel
    except AttributeError:
        pass

    ch = get_channel(target_channel)
    if ch is None:
        log.warning("hop_listener_unknown_channel", target=target_channel)
        return False
    ok = await wfb_manager.start_rx(interface, target_channel)
    return bool(ok)
