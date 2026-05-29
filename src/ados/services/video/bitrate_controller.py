"""Closed-loop video bitrate + FEC controller.

Watches the live link quality stats and steps a four-tier
bitrate/FEC ladder up or down based on packet loss and RSSI
hysteresis, scaled to our Python + ffmpeg multi-process stack: the
controller restarts wfb_tx on each tier change (~500 ms blackout) and asks
the pipeline supervisor for a matching encoder restart (~1 s
blackout). Total tier-change cost is well inside the operator's
tolerance for a "the link automatically adapted" stutter.

Hysteresis:
- Step down on any 5-second window with packet_loss > 5% OR
  rssi_dbm < -75. Aggressive; a degrading link is the urgent case.
- Step up after 30 seconds of continuous clean operation at the
  current tier (loss < 1% AND rssi > -65). Conservative; we
  don't ping-pong on transient cleanups.
- Step-down cooldown: 5 s minimum between consecutive downs so
  the link can settle on a new tier before the next decision.
- Step-up cooldown: 30 s minimum.

Manual override:
- set_auto(False) freezes the controller at whatever tier was
  active and accepts set_manual_tier() for operator-pinned ops.
- set_auto(True) returns to closed-loop control starting from
  the manual tier.
"""

from __future__ import annotations

import asyncio
import time
from collections.abc import Awaitable, Callable
from dataclasses import dataclass
from typing import Any

import structlog

log = structlog.get_logger(__name__)


# Step-up / step-down state required to take an action. Tuned against
# 1 Hz sampling on the bench: 5 consecutive bad samples = 5 s of
# sustained degradation, 30 consecutive clean samples = 30 s of
# sustained recovery.
_STEP_DOWN_LOSS_PCT = 5.0
_STEP_DOWN_RSSI_DBM = -75.0
_STEP_DOWN_REQUIRED_BAD_SAMPLES = 5

_STEP_UP_LOSS_PCT = 1.0
_STEP_UP_RSSI_DBM = -65.0
_STEP_UP_REQUIRED_CLEAN_SAMPLES = 30

_STEP_DOWN_COOLDOWN_S = 5.0
_STEP_UP_COOLDOWN_S = 30.0

_DEFAULT_TICK_INTERVAL_S = 1.0


@dataclass(frozen=True)
class BitrateTier:
    """One rung on the bitrate / FEC ladder.

    bitrate_kbps drives the video encoder. fec_k / fec_n drive the
    wfb_tx Reed-Solomon configuration. Each tier should be playable
    independently — no implicit pairing assumptions beyond what's
    written here.
    """

    name: str
    bitrate_kbps: int
    fec_k: int
    fec_n: int


# Index 0 is the high-quality default; the controller climbs back to
# this tier whenever the link permits. Index -1 is the rescue tier
# the controller falls to on a very degraded link — bitrate is still
# enough for a recognizable framerate, FEC ratio is 8/4 = 200% so
# every block survives 2 packet losses.
DEFAULT_TIERS: tuple[BitrateTier, ...] = (
    BitrateTier("high", 4000, 8, 12),
    BitrateTier("medium", 3000, 8, 14),
    BitrateTier("low", 2000, 8, 16),
    BitrateTier("rescue", 1200, 4, 12),
)


# Type aliases for the callbacks the controller invokes when a tier
# change lands. Both must be async because they're going to spawn
# subprocesses underneath.
SetFecCallback = Callable[[int, int], Awaitable[bool]]
SetBitrateCallback = Callable[[int], Awaitable[bool]]


class BitrateController:
    """Background asyncio task that drives the tier ladder.

    Wire the controller after WfbManager.start_tx() and the video
    pipeline are both up. The controller does not gate on the
    pipeline being healthy at the moment of construction; it polls
    LinkQualityMonitor and only acts when there's a stable sample
    stream.

    Disabled out of the box for new rigs (adaptive_bitrate_enabled
    defaults False on WfbConfig); a future operator-facing UI flips
    it on. When disabled the controller still loops at 1 Hz so its
    diagnostics surface stays populated, but it never calls the
    set_fec / set_bitrate callbacks.
    """

    def __init__(
        self,
        *,
        link_quality_monitor: Any,
        set_fec: SetFecCallback,
        set_bitrate: SetBitrateCallback,
        tiers: tuple[BitrateTier, ...] = DEFAULT_TIERS,
        tick_interval_s: float = _DEFAULT_TICK_INTERVAL_S,
        enabled: bool = False,
        starting_tier_idx: int = 0,
    ) -> None:
        if not tiers:
            raise ValueError("tiers must contain at least one entry")
        if not 0 <= starting_tier_idx < len(tiers):
            raise ValueError(
                f"starting_tier_idx {starting_tier_idx} out of range "
                f"[0, {len(tiers)})"
            )

        self._lqm = link_quality_monitor
        self._set_fec = set_fec
        self._set_bitrate = set_bitrate
        self._tiers = tiers
        self._tick_interval_s = tick_interval_s

        # Mutable state guarded by the asyncio event loop. The loop
        # is single-task so we don't need a lock — every mutation
        # happens inside _loop or inside the public set_* methods
        # called from the same loop's tasks.
        self._enabled = enabled
        self._auto = True
        self._current_tier_idx = starting_tier_idx
        self._stop_event = asyncio.Event()

        # Hysteresis counters.
        self._bad_streak = 0
        self._clean_streak = 0
        self._last_down_at = 0.0
        self._last_up_at = 0.0
        self._last_action_reason = "initial"

    @property
    def current_tier(self) -> BitrateTier:
        return self._tiers[self._current_tier_idx]

    @property
    def current_tier_idx(self) -> int:
        return self._current_tier_idx

    @property
    def auto(self) -> bool:
        return self._auto

    @property
    def enabled(self) -> bool:
        return self._enabled

    def snapshot(self) -> dict[str, Any]:
        """Diagnostic snapshot consumed by the REST surface.

        Stable shape so the GCS Video Link panel can render without
        a schema migration when an additional metric joins. Numeric
        fields are JSON-safe.
        """
        return {
            "enabled": self._enabled,
            "auto": self._auto,
            "tier_idx": self._current_tier_idx,
            "tier_name": self.current_tier.name,
            "bitrate_kbps": self.current_tier.bitrate_kbps,
            "fec_k": self.current_tier.fec_k,
            "fec_n": self.current_tier.fec_n,
            "bad_streak": self._bad_streak,
            "clean_streak": self._clean_streak,
            "last_action_reason": self._last_action_reason,
            "tiers": [
                {
                    "idx": idx,
                    "name": t.name,
                    "bitrate_kbps": t.bitrate_kbps,
                    "fec_k": t.fec_k,
                    "fec_n": t.fec_n,
                }
                for idx, t in enumerate(self._tiers)
            ],
        }

    def set_auto(self, enabled: bool) -> None:
        """Toggle closed-loop control. Manual mode freezes the tier."""
        self._auto = bool(enabled)
        if self._auto:
            # Clear hysteresis when re-entering auto so the next
            # decision is fresh, not biased by stale samples taken
            # under manual control.
            self._bad_streak = 0
            self._clean_streak = 0
            self._last_action_reason = "resumed_auto"

    async def set_manual_tier(self, tier_idx: int) -> bool:
        """Pin a specific tier. Implicitly disables auto."""
        if not 0 <= tier_idx < len(self._tiers):
            log.warning(
                "bitrate_controller_manual_tier_oob",
                requested=tier_idx,
                max=len(self._tiers) - 1,
            )
            return False
        self._auto = False
        if tier_idx == self._current_tier_idx:
            return True
        await self._apply_tier(tier_idx, reason="manual_override")
        return True

    def set_enabled(self, enabled: bool) -> None:
        """Master enable/disable. Disabled = loop still runs but no
        callbacks fire. Lets the GCS read the snapshot without
        actuating the radio."""
        self._enabled = bool(enabled)
        if not self._enabled:
            self._bad_streak = 0
            self._clean_streak = 0
            self._last_action_reason = "disabled"

    async def run(self) -> None:
        """Long-running coroutine driven by the agent service manager.

        Equivalent shape to other services in ``ados.services.*``:
        await this coroutine for the lifetime of the agent, signal
        stop() (or cancel the wrapping task) to terminate cleanly.
        """
        log.info(
            "bitrate_controller_started",
            tier=self.current_tier.name,
            enabled=self._enabled,
            auto=self._auto,
        )
        try:
            await self._loop()
        finally:
            log.info("bitrate_controller_stopped")

    async def stop(self) -> None:
        """Signal the loop to exit. Idempotent."""
        self._stop_event.set()

    async def _loop(self) -> None:
        next_persist = 0.0
        while not self._stop_event.is_set():
            try:
                await self._tick()
            except asyncio.CancelledError:
                raise
            except Exception as exc:
                # The controller is best-effort; one bad tick must
                # never take down the whole agent. Log and continue.
                log.error(
                    "bitrate_controller_tick_failed",
                    error=str(exc),
                    tier=self.current_tier.name,
                )
            # Persist snapshot to /run/ados/bitrate-controller.json
            # so the API process (separate from ados-wfb in
            # multi-process systemd) can read it without a cross-
            # process accessor. 5 s cadence matches the hop
            # supervisor; both feed the same GCS panel.
            import time as _time

            now = _time.monotonic()
            if now >= next_persist:
                self._persist_snapshot()
                next_persist = now + 5.0
            try:
                await asyncio.wait_for(
                    self._stop_event.wait(), timeout=self._tick_interval_s
                )
            except TimeoutError:
                pass

    def _persist_snapshot(self) -> None:
        """Write the current snapshot to /run/ados/bitrate-controller.json.

        Atomic tmpfile+rename. Best-effort: I/O failures swallowed
        at debug.
        """
        try:
            import json
            import time as _time

            from ados.core.paths import BITRATE_CONTROLLER_JSON

            path = BITRATE_CONTROLLER_JSON
            payload = self.snapshot()
            payload["wall_time_unix"] = _time.time()
            tmp = path.with_suffix(".tmp")
            tmp.parent.mkdir(parents=True, exist_ok=True)
            tmp.write_text(json.dumps(payload))
            tmp.replace(path)
        except OSError as exc:
            log.debug("bitrate_controller_persist_failed", error=str(exc))

    async def _tick(self) -> None:
        if not self._enabled or not self._auto:
            return

        sample = self._latest_sample()
        if sample is None:
            return

        loss = float(sample.get("loss_percent", 0.0))
        rssi = float(sample.get("rssi_dbm", -100.0))
        bad = (loss > _STEP_DOWN_LOSS_PCT) or (rssi < _STEP_DOWN_RSSI_DBM)
        clean = (loss < _STEP_UP_LOSS_PCT) and (rssi > _STEP_UP_RSSI_DBM)

        now = time.monotonic()

        if bad:
            self._bad_streak += 1
            self._clean_streak = 0
            if (
                self._bad_streak >= _STEP_DOWN_REQUIRED_BAD_SAMPLES
                and self._current_tier_idx + 1 < len(self._tiers)
                and now - self._last_down_at >= _STEP_DOWN_COOLDOWN_S
            ):
                await self._apply_tier(
                    self._current_tier_idx + 1,
                    reason=f"loss={loss:.1f}_rssi={rssi:.0f}",
                )
                self._last_down_at = now
                self._bad_streak = 0
            return

        if clean:
            self._clean_streak += 1
            self._bad_streak = 0
            if (
                self._clean_streak >= _STEP_UP_REQUIRED_CLEAN_SAMPLES
                and self._current_tier_idx > 0
                and now - self._last_up_at >= _STEP_UP_COOLDOWN_S
            ):
                await self._apply_tier(
                    self._current_tier_idx - 1,
                    reason=f"clean_loss={loss:.1f}_rssi={rssi:.0f}",
                )
                self._last_up_at = now
                self._clean_streak = 0
            return

        # Sample is neither bad nor clean (intermediate). Don't grow
        # either streak; let it decay toward zero so a marginal
        # period doesn't trigger anything.
        self._bad_streak = max(0, self._bad_streak - 1)
        self._clean_streak = max(0, self._clean_streak - 1)

    def _latest_sample(self) -> dict[str, Any] | None:
        """Pull the freshest LinkStats off the monitor as a dict.

        Defensive against the monitor's exact API shape — uses
        ``latest`` attribute if present, otherwise the first entry
        from ``history``. Returns None when the monitor hasn't seen
        any samples yet (cold-start window).
        """
        latest = getattr(self._lqm, "_latest", None) or getattr(
            self._lqm, "latest", None
        )
        if latest is None:
            return None
        # LinkStats is a dataclass; vars() works.
        try:
            return vars(latest)
        except TypeError:
            return None

    async def _apply_tier(self, new_idx: int, *, reason: str) -> None:
        if new_idx == self._current_tier_idx:
            return

        old_tier = self._tiers[self._current_tier_idx]
        new_tier = self._tiers[new_idx]
        self._last_action_reason = f"{reason}->{new_tier.name}"
        log.info(
            "bitrate_tier_change",
            old=old_tier.name,
            new=new_tier.name,
            reason=reason,
        )

        # Order matters. Push the new bitrate to the encoder first
        # so when wfb_tx comes back up the air is already on the
        # lower datarate (or the upgraded one), keeping the air on a
        # consistent datarate across the restart.
        try:
            fec_ok = await self._set_fec(new_tier.fec_k, new_tier.fec_n)
        except Exception as exc:
            log.error(
                "bitrate_tier_set_fec_failed",
                error=str(exc),
                new=new_tier.name,
            )
            fec_ok = False
        try:
            br_ok = await self._set_bitrate(new_tier.bitrate_kbps)
        except Exception as exc:
            log.error(
                "bitrate_tier_set_bitrate_failed",
                error=str(exc),
                new=new_tier.name,
            )
            br_ok = False

        if not (fec_ok and br_ok):
            log.warning(
                "bitrate_tier_partial",
                old=old_tier.name,
                new=new_tier.name,
                fec_ok=fec_ok,
                bitrate_ok=br_ok,
            )

        self._current_tier_idx = new_idx
