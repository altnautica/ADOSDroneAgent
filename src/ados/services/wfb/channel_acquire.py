"""Ground-side channel acquisition: sweep candidates until valid decode.

When the receiver comes up it has no way to know which channel the
transmitter is actually on. The configured channel can be wrong: the
transmitter may have hopped to a quieter frequency, the operator may
have changed the band, or a half-finished bind left the two sides on
different channels. Sitting on a single channel that the transmitter
is not using yields a permanently dry link with no error in the log.

This module sweeps the configured band's candidate channels, dwelling
briefly on each, and locks onto the first one where valid packets are
decoded (``packets_received`` increments). The valid-decode counter is
the only trustworthy signal: the interface ``rx_packets`` counter is
inflated by ambient RF the receiver cannot decode, so it never goes to
zero outdoors and cannot be used to tell "we hear our peer" from "we
hear noise".

The transmitter shortcuts this scan by advertising its current
operating channel in the control-plane presence beacon; when the
receiver hears that beacon it sets the announced channel directly and
verifies a valid decode rather than sweeping the whole band.

Acquisition is triggered on three events:

* post-bind (the channel on disk may be stale),
* valid-packet delta stays at zero for the silence window,
* a periodic tick while still unlocked.

Retries are bounded so a receiver with no peer in range does not sweep
forever; once the bound is hit the status reports ``no-peer`` and the
caller waits for the next trigger.
"""

from __future__ import annotations

import asyncio
import time
from collections.abc import Awaitable, Callable
from enum import StrEnum
from typing import Any

from ados.core.logging import get_logger
from ados.services.wfb.channel import _BAND_CHANNELS, STANDARD_CHANNELS

log = get_logger("wfb.channel_acquire")

# Per-channel dwell while sweeping. Long enough for the transmitter's
# next FEC block + the receiver's decode to land (the radio retune
# blackout is ~100-300 ms; a healthy stream emits a valid packet within
# a few hundred ms once tuned), short enough that a full nine-channel
# sweep finishes in well under ten seconds.
DWELL_SECONDS = 0.8

# Number of valid-packet samples to read inside each dwell. The receiver
# stats file is refreshed ~1 Hz; we poll it a few times per dwell so a
# decode that lands mid-dwell is caught before we move on.
_DWELL_POLLS = 4

# Silence window: valid-packet delta flat for this long while unlocked
# (or after a lock that went dry) triggers a fresh sweep. Matches the
# receive-liveness silence window so the two watchdogs agree.
VALID_PACKET_SILENCE_SECONDS = 12.0

# Periodic re-attempt cadence while unlocked and no peer beacon has
# pointed us at a channel.
PERIODIC_RETRY_SECONDS = 20.0

# Bound on consecutive full sweeps before reporting no-peer and pausing
# until the next external trigger. Keeps a receiver with no peer in
# range from burning the radio on an endless scan.
MAX_SWEEP_ROUNDS = 3


class AcquireState(StrEnum):
    """Acquisition lifecycle state, surfaced on the receiver status."""

    IDLE = "idle"
    SEARCHING = "searching"
    LOCKED = "locked"
    NO_PEER = "no-peer"


def candidate_channels(band: str) -> list[int]:
    """Channel numbers to sweep for ``band``, current-config-first order.

    The configured band's channels come first so the common case (the
    peer is on a channel inside the operator's chosen band) locks fast.
    Falls back to all standard channels when the band key is unknown.
    """
    band_numbers = _BAND_CHANNELS.get(band) or _BAND_CHANNELS["all"]
    ordered = list(band_numbers)
    # Append any remaining standard channels so a peer that hopped out
    # of band is still found, just later in the sweep.
    for ch in STANDARD_CHANNELS:
        if ch.channel_number not in ordered:
            ordered.append(ch.channel_number)
    return ordered


async def _set_channel(interface: str, channel: int) -> bool:
    """Retune ``interface`` to ``channel`` via the monitor-mode iw path.

    Async sibling of the hop listener's channel set so acquisition can
    run inside the receiver's asyncio loop without blocking it. Returns
    True when iw reports success.
    """
    try:
        proc = await asyncio.create_subprocess_exec(
            "iw",
            interface,
            "set",
            "channel",
            str(channel),
            stdout=asyncio.subprocess.DEVNULL,
            stderr=asyncio.subprocess.PIPE,
        )
        _out, err = await asyncio.wait_for(proc.communicate(), timeout=5.0)
    except (FileNotFoundError, TimeoutError, OSError) as exc:
        log.warning("acquire_set_channel_error", channel=channel, error=str(exc))
        return False
    if proc.returncode != 0:
        log.warning(
            "acquire_set_channel_failed",
            channel=channel,
            stderr=(err.decode(errors="replace").strip() if err else ""),
        )
        return False
    return True


class ChannelAcquirer:
    """Sweep candidate channels until a valid decode locks the link.

    The acquirer is profile-agnostic in mechanics but only meaningful on
    the receive side, where there is a transmitter to find. It reads the
    current valid-packet counter through a caller-supplied callback so it
    stays decoupled from the receiver manager's internals and is trivial
    to unit-test with a synthetic counter series.
    """

    def __init__(
        self,
        *,
        interface: str,
        band: str,
        valid_packets_fn: Callable[[], int],
        set_channel_fn: Callable[[str, int], Awaitable[bool]] | None = None,
        dwell_seconds: float = DWELL_SECONDS,
        max_sweep_rounds: int = MAX_SWEEP_ROUNDS,
    ) -> None:
        self._interface = interface
        self._band = band
        self._valid_packets_fn = valid_packets_fn
        self._set_channel_fn = set_channel_fn or _set_channel
        self._dwell_seconds = dwell_seconds
        self._max_sweep_rounds = max_sweep_rounds
        self._state = AcquireState.IDLE
        self._locked_channel: int | None = None
        self._last_attempt_at: float = 0.0
        # Serializes radio retunes. The watchdog's full-band acquire()
        # and the per-beacon acquire_target() both drive `iw set channel`
        # on the same interface; running them concurrently would fight
        # over the radio and corrupt each other's dwell measurement. The
        # lock is held across an entire acquire / acquire_target / public
        # try_channel call so a sweep is never interleaved with a beacon
        # verify.
        self._lock = asyncio.Lock()

    @property
    def in_progress(self) -> bool:
        """True while a retune (sweep or beacon verify) is in flight."""
        return self._lock.locked()

    @property
    def state(self) -> AcquireState:
        return self._state

    @property
    def locked_channel(self) -> int | None:
        return self._locked_channel

    @property
    def channel_locked(self) -> bool:
        return self._state == AcquireState.LOCKED

    def status(self) -> dict[str, Any]:
        """Acquisition snapshot for the receiver status surface."""
        return {
            "acquire_state": self._state.value,
            "channel_locked": self.channel_locked,
            "locked_channel": self._locked_channel,
        }

    def mark_unlocked(self) -> None:
        """Drop the lock so the next trigger sweeps again.

        Called by the receive-liveness watchdog when a previously locked
        link goes silent — the transmitter may have hopped away.
        """
        if self._state == AcquireState.LOCKED:
            log.info("acquire_lock_dropped", channel=self._locked_channel)
        self._state = AcquireState.SEARCHING
        self._locked_channel = None

    def mark_locked(self, channel: int) -> None:
        """Record that valid video is decoding on ``channel``.

        A sweep is only ONE way the link becomes locked. A rig that boots
        already tuned to the persisted channel (the common case) never
        runs a sweep, yet it is plainly locked the moment valid decodes
        flow. The receive path calls this when packets are arriving so
        the lock state reflects reality — "I am decoding valid video on
        this channel" — instead of staying IDLE until an explicit sweep.
        Idempotent and lock-free (a plain state assignment) so it is safe
        to call from the per-stats-line hot path.
        """
        if self._state != AcquireState.LOCKED or self._locked_channel != channel:
            log.info("acquire_locked_on_decode", channel=channel)
        self._state = AcquireState.LOCKED
        self._locked_channel = channel

    async def _try_channel_locked(self, channel: int) -> bool:
        """Retune + dwell on ``channel`` (caller must hold ``_lock``).

        Returns True if the valid-packet counter advances within the
        dwell window (the peer is on this channel and we are decoding
        it), False otherwise. On success the acquirer is left LOCKED on
        ``channel``.
        """
        baseline = int(self._valid_packets_fn())
        ok = await self._set_channel_fn(self._interface, channel)
        if not ok:
            return False
        per_poll = max(self._dwell_seconds / _DWELL_POLLS, 0.0)
        for _ in range(_DWELL_POLLS):
            await asyncio.sleep(per_poll)
            if int(self._valid_packets_fn()) > baseline:
                self._state = AcquireState.LOCKED
                self._locked_channel = channel
                log.info("acquire_locked", interface=self._interface, channel=channel)
                return True
        return False

    async def try_channel(self, channel: int) -> bool:
        """Tune to ``channel`` and watch for a valid-packet increment.

        Public entry point for standalone callers. Holds ``_lock`` so a
        lone retune cannot interleave with a sweep or beacon verify.
        """
        async with self._lock:
            return await self._try_channel_locked(channel)

    async def acquire(self) -> int | None:
        """Sweep the band until a channel decodes valid packets.

        Returns the locked channel number, or None when no candidate
        produced a valid decode within the sweep bound (status left at
        ``no-peer``). Holds ``_lock`` across the whole sweep so a
        concurrent beacon verify cannot fight it for the radio.
        """
        async with self._lock:
            self._last_attempt_at = time.monotonic()
            self._state = AcquireState.SEARCHING
            channels = candidate_channels(self._band)
            log.info(
                "acquire_sweep_start",
                interface=self._interface,
                band=self._band,
                candidates=channels,
            )
            for _round in range(self._max_sweep_rounds):
                for channel in channels:
                    if await self._try_channel_locked(channel):
                        return channel
            self._state = AcquireState.NO_PEER
            log.warning(
                "acquire_no_peer",
                interface=self._interface,
                band=self._band,
                rounds=self._max_sweep_rounds,
            )
            return None

    async def acquire_target(self, channel: int) -> bool:
        """Verify a beacon-announced channel with a single dwell.

        The transmitter advertises its operating channel in the presence
        beacon; rather than sweep the whole band the receiver tunes to
        that channel and confirms a valid decode. Holds ``_lock`` so it
        cannot race a concurrent sweep. Falls back to the caller's normal
        sweep trigger when the announced channel does not decode (the
        beacon may be stale or the peer just hopped again).
        """
        async with self._lock:
            self._last_attempt_at = time.monotonic()
            self._state = AcquireState.SEARCHING
            log.info(
                "acquire_beacon_target",
                interface=self._interface,
                channel=channel,
            )
            return await self._try_channel_locked(channel)
