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
import subprocess

from ados.core.paths import HOP_SUPERVISOR_JSON
import struct
import time
from dataclasses import dataclass
from typing import Any

import structlog

from ados.services.wfb.adapter import (
    enabled_channels,
    get_interface_mode,
    set_monitor_mode,
)
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

# After the link has been up, this is how long the peer can be silent
# before the drone gives up on the current channel and returns to the
# home / rendezvous channel so the two rigs can re-meet. Two missed
# presence-beacon intervals (~10 s each) plus margin.
_HOP_PEER_STALE_S = 25.0


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


# PresenceBeacon — periodic "I am here, I am peer X" frame on the same
# WFB control plane (radio_id 1, UDP 5803 loopback ingress) as the
# HopAnnounce. Disambiguated from HopAnnounce by the magic prefix
# (PresenceBeacon = b"AD05PRES", HopAnnounce = b"AD05HOP1"). Replaces
# the mDNS-based inter-rig discovery dependency.
_PRESENCE_MAGIC = b"AD05PRES"
_PRESENCE_VERSION_CURRENT = 1
_PRESENCE_ROLE_DRONE = 0x01
_PRESENCE_ROLE_GS = 0x02
_PRESENCE_DEVICE_ID_BYTES = 16
_PRESENCE_BEACON_TOTAL_BYTES = 68


@dataclass(frozen=True)
class PresenceBeacon:
    """Wire shape of a presence beacon.

    Layout (68 bytes total):
      offset 0..8    magic (b"AD05PRES")
      offset 8       version (1B, currently 0x01)
      offset 9..25   device_id (16B, hex device-id ASCII zero-padded)
      offset 25      role (1B, 0x01 drone / 0x02 gs)
      offset 26      channel (1B, current radio channel)
      offset 27      rssi (signed int8, 0 if unknown)
      offset 28..36  epoch_ms (uint64 BE, monotonic per rig)
      offset 36..68  hmac_sha256 (32B) over the preceding 36 bytes

    HMAC keyed by the same `/etc/drone.key`-derived secret as
    HopAnnounce — `_resolve_pair_key()`. A third party watching the
    control band cannot forge a beacon without that key.
    """

    version: int
    device_id: str
    role: int
    channel: int
    rssi_dbm: int
    epoch_ms: int

    def encode(self, pair_key: bytes) -> bytes:
        device_id_bytes = self.device_id.encode("ascii", errors="ignore")[
            :_PRESENCE_DEVICE_ID_BYTES
        ].ljust(_PRESENCE_DEVICE_ID_BYTES, b"\x00")
        rssi_byte = max(-128, min(127, int(self.rssi_dbm))) & 0xFF
        body = (
            _PRESENCE_MAGIC
            + bytes([self.version])
            + device_id_bytes
            + bytes([self.role & 0xFF])
            + bytes([self.channel & 0xFF])
            + bytes([rssi_byte])
            + struct.pack(">Q", self.epoch_ms)
        )
        tag = hmac.new(pair_key, body, hashlib.sha256).digest()
        return body + tag

    @classmethod
    def decode(cls, raw: bytes, pair_key: bytes) -> "PresenceBeacon | None":
        if len(raw) != _PRESENCE_BEACON_TOTAL_BYTES:
            return None
        if not raw.startswith(_PRESENCE_MAGIC):
            return None
        body, tag = raw[:-32], raw[-32:]
        expected = hmac.new(pair_key, body, hashlib.sha256).digest()
        if not hmac.compare_digest(expected, tag):
            return None
        version = body[8]
        device_id_bytes = body[9:25].rstrip(b"\x00")
        try:
            device_id = device_id_bytes.decode("ascii")
        except UnicodeDecodeError:
            return None
        role = body[25]
        channel = body[26]
        rssi_raw = body[27]
        rssi_dbm = rssi_raw - 256 if rssi_raw >= 128 else rssi_raw
        epoch_ms = struct.unpack(">Q", body[28:36])[0]
        return cls(
            version=version,
            device_id=device_id,
            role=role,
            channel=channel,
            rssi_dbm=rssi_dbm,
            epoch_ms=epoch_ms,
        )

    @property
    def role_label(self) -> str:
        return {
            _PRESENCE_ROLE_DRONE: "drone",
            _PRESENCE_ROLE_GS: "gs",
        }.get(self.role, "unknown")


def _resolve_pair_key() -> bytes:
    """Derive the symmetric pair key used to authenticate HopAnnounce.

    wfb-ng's bind protocol produces a libsodium crypto_box keypair
    (`/etc/drone.key` + `/etc/gs.key`, 64 bytes each, NOT byte-identical).
    The wfb_bind_server.sh script on the receiving rig writes the
    drone-side file to `/etc/drone.key`, so AFTER a successful bind both
    rigs have a `/etc/drone.key` with the SAME bytes — that file is the
    only shared-content key file on disk and is the right source for a
    symmetric HMAC derivation.

    Previous versions hashed `/etc/ados/wfb/tx.key` on the drone and
    `/etc/ados/wfb/rx.key` on the GS. Those are the two DIFFERENT halves
    of the crypto_box pair (drone gets `drone.key` renamed, GS gets
    `gs.key` renamed), so the derived HMAC key diverged across the rigs
    and every HopAnnounce was silently dropped at the listener.
    """
    candidates = (
        # Canonical: the file wfb-ng's bind protocol delivers byte-for-
        # byte to both rigs.
        "/etc/drone.key",
        # Forward-compatibility: if a future migration relocates the
        # file inside the agent's namespace.
        "/etc/ados/wfb/drone.key",
    )
    for path in candidates:
        try:
            with open(path, "rb") as f:
                key_bytes = f.read()
        except (OSError, FileNotFoundError):
            continue
        if len(key_bytes) != 64:
            continue
        h = hashlib.sha256()
        h.update(b"ados/wfb/hop/v2\n")
        h.update(key_bytes)
        return h.digest()
    # No keys on disk yet (cold start before bind). Use a constant
    # so a stray hop announce can still be parsed; the supervisor
    # gates on a successful pair before doing anything anyway.
    log.warning("hop_supervisor_pair_key_unavailable")
    return hashlib.sha256(b"ados/wfb/hop/v2/cold-start").digest()


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

    # Constrain candidates to channels this adapter actually enables.
    # The drone and ground frequently run different regulatory domains;
    # announcing a hop to a channel the ground (or the drone) cannot use
    # would split the pair onto different frequencies and break the link.
    # An empty set means "could not determine" (e.g. iw unreadable), in
    # which case we do not restrict and fall back to the band set alone.
    local_enabled = enabled_channels(interface)
    if local_enabled:
        band_set &= local_enabled
        if not band_set:
            log.info(
                "hop_supervisor_no_enabled_band_channels",
                band=band,
                note="no band channel is locally enabled; no hop",
            )
            return None

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
        home_channel: int = 149,
    ) -> None:
        self._wfb = wfb_manager
        self._lqm = link_quality_monitor
        self._band = band
        self._hop_period_s = max(15, int(hop_period_seconds))
        self._loss_threshold = float(loss_threshold_percent)
        self._rssi_threshold = float(rssi_threshold_dbm)
        self._enabled = bool(enabled)
        self._control_port = control_port
        # The fixed rendezvous channel both rigs come up on and return to
        # when the link is lost. After being linked, a sustained loss
        # with no peer ACK pulls the drone back here so the ground (which
        # also falls back to home) can re-find it.
        self._home_channel = int(home_channel)
        self._stop_event = asyncio.Event()
        self._last_hop_at: float = 0.0
        self._reactive_cooldown_s = 30.0
        self._history: list[dict[str, Any]] = []
        # Tracks whether a peer was ever seen this session, so a peer
        # that goes stale after being linked triggers the home-channel
        # fallback (vs cold start, which simply waits on the home
        # channel without hopping).
        self._was_linked: bool = False

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
            # Compute periodic_due and advance the threshold BEFORE
            # calling _tick. The prior structure deferred the advance
            # to the bottom of the loop, after the post-sleep `now` had
            # already crossed `next_periodic` — by the time _tick ran on
            # the next iteration the threshold was already pushed 60s
            # into the future, so `now >= next_periodic_at` inside _tick
            # was never True and the periodic hop never fired.
            now = time.monotonic()
            periodic_due = now >= next_periodic
            if periodic_due:
                next_periodic = now + self._hop_period_s
            try:
                await self._tick(periodic_due=periodic_due)
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

    async def _tick(self, periodic_due: bool) -> None:
        # Defensive: never trigger a channel change while a local bind
        # session is in flight. The bind orchestrator stops the normal
        # wfb unit so wfb-ng's bind profile can own the radio adapter
        # exclusively; a racing iw-channel + wfb_tx restart from this
        # supervisor would fight the bind tunnel and corrupt the socat
        # key exchange. Lazy import keeps this module independent of
        # the bind orchestrator at import time.
        now = time.monotonic()
        # `periodic_due` is computed and pre-advanced by _loop; reuse
        # the name `periodic` to keep the rest of this function readable.
        periodic = periodic_due

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

        # Rendezvous-first gate: do NOT scan or hop until the link has
        # been established at least once. Both rigs come up on the fixed
        # home channel and bring the link up there; only after a peer is
        # seen (a decoded PresenceBeacon / HopAck) does coordinated
        # hopping turn on. Before that the drone must stay on the home
        # channel transmitting so the ground can find it. Running an
        # `iw scan` here would strand the iface in managed mode and pull
        # the radio away from the home channel, which is the exact
        # divergence that kept the two sides from ever meeting. The
        # signal is `_peer_last_seen_unix`: None until the control plane
        # decodes the first frame from the peer.
        peer_last_seen = getattr(self._wfb, "_peer_last_seen_unix", None)
        link_established = isinstance(peer_last_seen, (int, float)) and (
            peer_last_seen > 0
        )
        if not link_established:
            if periodic:
                log.info(
                    "hop_supervisor_skip_unlinked",
                    note=(
                        "no peer seen yet; staying on home channel, "
                        "no scan until the link is established"
                    ),
                )
            return

        # Record that we have been linked this session. Used below to
        # tell a fresh cold start (never linked) apart from a link that
        # went away after being up (which should fall back to home).
        self._was_linked = True
        peer_age_s = time.time() - float(peer_last_seen)

        # Home-channel fallback: the link was up but the peer has gone
        # quiet past the freshness window. The drone may have hopped off
        # the home channel; the ground rig falls back to home on its own
        # loss, so the drone must return there too for the two to re-meet.
        # Do this before any scan so a lost link converges on the
        # rendezvous channel instead of chasing a quiet channel the peer
        # is no longer on.
        if (
            peer_age_s >= _HOP_PEER_STALE_S
            and self._wfb._channel != self._home_channel
        ):
            log.warning(
                "hop_supervisor_fallback_home",
                from_channel=self._wfb._channel,
                home_channel=self._home_channel,
                peer_last_seen_ago_s=round(peer_age_s, 1),
                note="link lost after being up; returning to home channel",
            )
            await self._return_to_home()
            return

        # Link is up. Skip the periodic scan while the peer is still
        # fresh: _pick_target_channel below shells out to `iw <iface>
        # scan` which locks the radio for ~6 s, dropping the wfb_tx
        # broadcast frames that share the same wlan0. When the control
        # plane decoded a PresenceBeacon in the last minute the link is
        # fine and the rescan is pure waste, so return early. Reactive
        # scans (loss / RSSI thresholds tripped) always run because that
        # is the only way to find a better channel.
        if periodic and not reactive:
            if (time.time() - peer_last_seen) < 60.0:
                log.info(
                    "hop_supervisor_skip_periodic_link_healthy",
                    peer_last_seen_ago_s=round(
                        time.time() - peer_last_seen, 1
                    ),
                )
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
        # _pick_target_channel shells out to `iw scan`, which can leave
        # the iface in a non-monitor/wedged state on some drivers. The
        # link is up and wfb_tx must keep injecting, so re-assert monitor
        # mode immediately after the scan regardless of whether a hop
        # follows. The channel is set again by _execute_hop on a hop, or
        # restored to the current channel here when there is no candidate.
        self._restore_monitor_after_scan()
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

    def _restore_monitor_after_scan(self) -> None:
        """Re-assert monitor mode + current channel after an iw scan.

        A scan can leave the iface in managed mode (the scan path needs
        it) or otherwise wedged on some drivers, which would silently
        stop wfb_tx injection. The link is up, so put the iface back in
        monitor mode and retune to the channel wfb_tx expects whenever
        the observed mode is not monitor. Best-effort and synchronous,
        the cost is one iw round-trip on the rare non-monitor case.
        """
        interface = getattr(self._wfb, "_interface", None)
        if not interface:
            return
        mode = get_interface_mode(interface)
        if mode == "monitor" or mode is None:
            return
        log.warning(
            "hop_supervisor_monitor_restored_after_scan",
            interface=interface,
            observed_mode=mode,
        )
        if set_monitor_mode(interface):
            try:
                subprocess.run(
                    [
                        "iw",
                        interface,
                        "set",
                        "channel",
                        str(self._wfb._channel),
                    ],
                    capture_output=True,
                    timeout=5,
                )
            except (FileNotFoundError, subprocess.TimeoutExpired, OSError) as exc:
                log.debug(
                    "hop_supervisor_channel_restore_failed",
                    error=str(exc),
                )

    async def _return_to_home(self) -> None:
        """Move the radio back to the home / rendezvous channel.

        Used when a previously-established link has gone quiet. Unlike a
        hop this needs no announce + ACK handshake: the ground rig falls
        back to the same home channel on its own loss, so both sides
        independently converge on the rendezvous frequency where they
        first met. Mechanics mirror the channel-flip in _execute_hop:
        stop the data plane, set the channel via iw, then bring wfb_tx
        and the control plane back up on the home channel.
        """
        home = self._home_channel
        await self._wfb.stop()
        await asyncio.create_subprocess_exec(
            "iw",
            self._wfb._interface,
            "set",
            "channel",
            str(home),
            stdout=asyncio.subprocess.DEVNULL,
            stderr=asyncio.subprocess.DEVNULL,
        )
        self._wfb._channel = home
        ok = await self._wfb.start_tx(self._wfb._interface, home)
        start_tx_control = getattr(self._wfb, "start_tx_control", None)
        start_rx_control = getattr(self._wfb, "start_rx_control", None)
        if start_tx_control is not None:
            await start_tx_control(self._wfb._interface, home)
        if start_rx_control is not None:
            await start_rx_control(self._wfb._interface, home)
        self._history.append(
            {
                "at": time.time(),
                "from": self._wfb._channel,
                "to": home,
                "trigger": "fallback_home",
                "ok": bool(ok),
            }
        )
        log.info("hop_supervisor_returned_home", home=home, tx_ok=ok)

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
        # Persist + bring wfb back up on the new channel. Restart the
        # control-plane subprocesses alongside the data plane so the
        # next periodic HopAnnounce after this hop can travel over the
        # WFB radio. If a control-plane spawn fails the data plane
        # stays up; the hop is simply deferred until the next periodic
        # tick when the control plane is healthy again.
        self._wfb._channel = target_channel
        ok = await self._wfb.start_tx(self._wfb._interface, target_channel)
        start_tx_control = getattr(self._wfb, "start_tx_control", None)
        start_rx_control = getattr(self._wfb, "start_rx_control", None)
        if start_tx_control is not None:
            await start_tx_control(self._wfb._interface, target_channel)
        if start_rx_control is not None:
            await start_rx_control(self._wfb._interface, target_channel)
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
        """Repeatedly broadcast the announce and wait for HopAck.

        ADOS is local-first / field-deployed; the inter-rig link IS the
        WFB radio. HopAnnounce goes out over the WFB control plane
        (radio_id 1): the drone writes the encoded frame to
        127.0.0.1:5803 — wfb_tx_control's UDP ingress — and the GS-side
        wfb_rx_control decodes the frame off the air and emits it on
        UDP 5803 where HopListener binds. HopAck travels the reverse
        path: HopListener writes the ACK to 127.0.0.1:5810 (the GS-side
        wfb_tx_control ingress) and the drone-side wfb_rx_control
        decodes and emits on UDP 5810 here.

        No LAN / operator-network destinations: consumer APs frequently
        drop limited broadcasts and bridge wired↔wireless unreliably,
        and the operator's LAN is irrelevant once the drone is in the
        air. The WFB radio is the only inter-rig transport that is
        guaranteed to be present in field conditions.
        """
        ack_event = asyncio.Event()
        target_channel = announce.target_channel

        # Send socket bound to an ephemeral source port (0). The
        # destination loopback port 5803 is wfb_tx_control's UDP
        # ingress; binding 5803 here would race against that subprocess
        # for kernel delivery on outgoing `sendto(127.0.0.1, 5803)`.
        sock_send = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        try:
            sock_send.bind(("0.0.0.0", 0))
            sock_send.setblocking(False)
        except OSError as exc:
            log.warning("hop_supervisor_socket_failed", error=str(exc))
            sock_send.close()
            return False

        # Register the ACK event with the WfbManager's control-plane
        # listener. The listener is the single long-running consumer of
        # UDP 5810 (where wfb_rx_control delivers every -p 1 frame the
        # radio decodes) and routes incoming HopAcks to whichever
        # channel-keyed event is currently registered.
        ack_events = getattr(self._wfb, "_ack_events", None)
        if ack_events is not None:
            ack_events[target_channel] = ack_event

        payload = announce.encode(pair_key)
        destinations: list[tuple[str, int]] = [("127.0.0.1", 5803)]
        log.info(
            "hop_supervisor_broadcast_start",
            target=target_channel,
            destinations=[d[0] for d in destinations],
        )
        try:
            for _ in range(_HOP_BROADCAST_ROUNDS):
                if ack_event.is_set():
                    break
                for dest in destinations:
                    try:
                        sock_send.sendto(payload, dest)
                    except OSError as exc:
                        log.debug(
                            "hop_supervisor_send_failed",
                            dest=dest[0],
                            error=str(exc),
                        )
                await asyncio.sleep(HOP_BROADCAST_INTERVAL_MS / 1000.0)
        finally:
            sock_send.close()
            if ack_events is not None:
                ack_events.pop(target_channel, None)
        if ack_event.is_set():
            log.info(
                "hop_supervisor_ack_received",
                target=target_channel,
            )
            return True
        return False


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
        # Presence cache, populated by inbound PresenceBeacons. The
        # heartbeat consumer reads this without taking a lock; reads are
        # whole-dict copies under GIL so partial torn reads are not a
        # concern at the field granularity here.
        self._peer_device_id: str | None = None
        self._peer_role: str | None = None
        self._peer_channel: int | None = None
        self._peer_rssi_dbm: int | None = None
        self._peer_last_seen_unix: float | None = None

    def get_peer_presence(self) -> dict[str, Any]:
        """Snapshot the current peer presence state (heartbeat consumer)."""
        return {
            "peer_device_id": self._peer_device_id,
            "peer_role": self._peer_role,
            "peer_channel": self._peer_channel,
            "peer_rssi_dbm": self._peer_rssi_dbm,
            "peer_last_seen_unix": self._peer_last_seen_unix,
        }

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

    def _persist_peer_presence(self) -> None:
        """Write the peer presence snapshot to /run/ados/peer-presence.json.

        The heartbeat builder in the main agent process reads this file
        cross-process to enrich the cloud-relay heartbeat with the peer
        device-id / role / channel / RSSI / freshness, since the
        HopListener lives in ados-wfb-rx and the heartbeat lives in
        ados-api.
        """
        from ados.core.paths import PEER_PRESENCE_JSON
        payload = self.get_peer_presence()
        try:
            tmp = PEER_PRESENCE_JSON.with_suffix(".tmp")
            tmp.parent.mkdir(parents=True, exist_ok=True)
            tmp.write_text(json.dumps(payload))
            tmp.replace(PEER_PRESENCE_JSON)
        except OSError as exc:
            log.debug("peer_presence_persist_failed", error=str(exc))

    def _maybe_follow_peer_channel(self, peer_channel: int) -> None:
        """Tune to the peer's announced channel — only if we're not
        already decoding it.

        The transmitter advertises its operating channel in the presence
        beacon. But hearing that beacon at all means we are already on
        the right control-plane channel, so following the announced
        channel blindly can retune AWAY from a working link. Only follow
        when we are NOT decoding valid video (valid-rx rate is zero):
        that is the case where we are mistuned and the beacon's channel
        is the hint we need. If video is flowing, stay put.

        Also no-op when an acquisition is already in flight (the
        watchdog's sweep holds the acquirer lock) so a beacon verify
        never races a sweep on the same radio.
        """
        if peer_channel not in _CHANNEL_NUMBERS:
            return
        current = getattr(self._wfb, "_channel", None)
        if current == peer_channel:
            return
        acquirer = getattr(self._wfb, "_acquirer", None)
        if acquirer is None:
            return
        # Already running a sweep / verify — don't pile on.
        if getattr(acquirer, "in_progress", False):
            return
        # If we are decoding valid video, we are on the right channel;
        # do not retune away from a working link.
        rate = getattr(self._wfb, "_valid_rx_packets_per_s", None)
        if isinstance(rate, (int, float)) and rate > 0:
            return
        try:
            loop = asyncio.get_running_loop()
        except RuntimeError:
            return

        async def _verify() -> None:
            try:
                ok = await acquirer.acquire_target(peer_channel)
                if ok:
                    try:
                        self._wfb._channel = peer_channel
                    except AttributeError:
                        pass
                    persist = getattr(
                        self._wfb, "_persist_locked_channel", None
                    )
                    if callable(persist):
                        persist(peer_channel)
                    log.info(
                        "hop_listener_followed_peer_channel",
                        channel=peer_channel,
                    )
            except Exception as exc:  # noqa: BLE001
                log.debug(
                    "hop_listener_follow_peer_failed",
                    channel=peer_channel,
                    error=str(exc),
                )

        loop.create_task(_verify())

    def _handle_presence(self, beacon: "PresenceBeacon") -> None:
        """Update the in-memory peer cache and back-fill pair state."""
        # Reject self-beacons. Loopback delivery can race against the
        # wfb_tx_control bind when the emit loop misroutes; rather than
        # rely on perfect port topology elsewhere, drop any beacon
        # that claims our own device-id outright.
        try:
            from ados.core.identity import get_or_create_device_id
            own_device_id = get_or_create_device_id()
        except Exception:
            own_device_id = None
        if own_device_id and beacon.device_id == own_device_id[:16]:
            return
        previous_device_id = self._peer_device_id
        self._peer_device_id = beacon.device_id
        self._peer_role = beacon.role_label
        self._peer_channel = int(beacon.channel)
        self._peer_rssi_dbm = int(beacon.rssi_dbm)
        self._peer_last_seen_unix = time.time()
        self._persist_peer_presence()

        # Short-circuit a band sweep: the peer beacon carries the
        # transmitter's current operating channel. When we hear a beacon
        # announcing a channel that differs from where we are tuned, set
        # that channel directly and verify a valid decode rather than
        # sweeping the whole band. Best-effort and non-blocking: schedule
        # the verify as a background task so the listener loop is never
        # held up by a channel retune.
        self._maybe_follow_peer_channel(int(beacon.channel))
        if previous_device_id != beacon.device_id:
            log.info(
                "hop_listener_peer_seen",
                peer_device_id=beacon.device_id,
                peer_role=beacon.role_label,
                channel=beacon.channel,
                rssi_dbm=beacon.rssi_dbm,
            )
            # Back-fill paired_with_device_id when the bind tunnel did
            # not carry it. Determine the local role from the wfb
            # manager: drone-side has no rx.key consumer, GS-side has
            # the rx_key path present.
            try:
                from ados.services.ground_station.pair_manager import (
                    update_peer_device_id,
                )
            except ImportError:
                return
            local_role: str = getattr(self._wfb, "_role", None) or (
                "gs" if getattr(self._wfb, "_is_ground_station", False)
                else "drone"
            )
            try:
                updated = update_peer_device_id(local_role, beacon.device_id)
                if updated:
                    log.info(
                        "pair_state_peer_back_filled",
                        peer_device_id=beacon.device_id,
                        local_role=local_role,
                    )
            except OSError as exc:
                log.debug(
                    "pair_state_peer_back_fill_failed", error=str(exc)
                )

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
        decode_failed_count: int = 0
        last_decode_failed_log: float = 0.0
        try:
            while not stop_event.is_set():
                try:
                    data, _peer = await asyncio.wait_for(
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

                # Dispatch by magic prefix — the same UDP 5803 socket
                # carries both HopAnnounce (51 B, magic AD05HOP1) and
                # PresenceBeacon (68 B, magic AD05PRES). wfb_rx_control
                # delivers either format here; we route based on the
                # first eight bytes.
                if data.startswith(_PRESENCE_MAGIC):
                    beacon = PresenceBeacon.decode(data, pair_key)
                    if beacon is None:
                        continue
                    self._handle_presence(beacon)
                    continue

                announce = HopAnnounce.decode(data, pair_key)
                if announce is None:
                    decode_failed_count += 1
                    now_mono = time.monotonic()
                    if now_mono - last_decode_failed_log >= 30.0:
                        log.warning(
                            "hop_listener_decode_failed",
                            count=decode_failed_count,
                            note=(
                                "received UDP frame on the hop control port "
                                "but neither HopAnnounce nor PresenceBeacon "
                                "decode accepted it (length, magic, or HMAC "
                                "mismatch); rate-limited 30s"
                            ),
                        )
                        last_decode_failed_log = now_mono
                        decode_failed_count = 0
                    continue
                if announce.target_channel not in _CHANNEL_NUMBERS:
                    continue
                # ACK by echoing the same packet back to 127.0.0.1:5810,
                # the loopback ingress of the local wfb_tx_control
                # subprocess (radio_id 1). wfb_tx_control transmits the
                # ACK over the WFB radio; the peer's wfb_rx_control
                # decodes it and emits on UDP 5810 where HopSupervisor's
                # ACK reader picks it up.
                try:
                    sock.sendto(data, ("127.0.0.1", 5810))
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
    # the critical difference vs calling stop() / stop_rx(). Also
    # tear down the control-plane subprocesses (radio_id 1) so they
    # re-acquire monitor frames on the new channel; they have to
    # restart in lock-step with the data plane.
    for attr in ("_rx_proc", "_rx_control_proc", "_tx_control_proc"):
        proc = getattr(wfb_manager, attr, None)
        if proc is None or getattr(proc, "returncode", 0) is not None:
            continue
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
        try:
            setattr(wfb_manager, attr, None)
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
    # Restart the GS-side control-plane subprocesses (radio_id 1) so
    # the next HopAnnounce / HopAck pair after this hop travels over
    # the radio link rather than the LAN. Failures are non-fatal —
    # the data plane (video receive) stays up.
    start_rx_control = getattr(wfb_manager, "start_rx_control", None)
    start_tx_control = getattr(wfb_manager, "start_tx_control", None)
    if start_rx_control is not None:
        await start_rx_control(interface, target_channel)
    if start_tx_control is not None:
        await start_tx_control(interface, target_channel)
    return bool(ok)
