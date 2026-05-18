"""Self-heal switch that disables the GStreamer air pipeline when its
bus error counter climbs faster than a healthy pipeline ever would.

Why this exists
---------------
The in-process GStreamer pipeline that wraps ``mpph264enc`` on
Rockchip boards is a per-board default that, in the happy case, cuts
encoder CPU from ~48 % to <10 %. The MPP kernel driver and the
gstreamer-rockchip plugin do not always cooperate cleanly across
combinations of (Linux kernel build, libmpp release, BSP image,
adapter firmware). When they don't, the pipeline still runs — but the
GStreamer bus emits steady error messages and the H.264 output gets
rejected by ``h264parse`` downstream, freezing the operator's video
without any obvious crash signal.

The watcher in this module observes the AirPipeline's ``bus_errors``
counter, decides whether the rate exceeds a healthy ceiling, and
writes a runtime override at ``/run/ados/video-encoder-override.yaml``
that flips ``use_gst_air_pipeline`` to ``false`` for subsequent
``VideoConfig`` loads. The override lives on ``/run`` (tmpfs) so a
reboot clears it — a transient bug never permanently disables
hardware encoding, but a persistent one keeps the fallback in place
for as long as the symptom holds.

The operator's explicit setting in ``/etc/ados/config.yaml`` always
wins: this override only acts when the field is unset and we computed
the per-board default. See ``video._default_use_gst_air_pipeline``.
"""

from __future__ import annotations

import time
from collections import deque
from pathlib import Path
from typing import Any

import yaml

from ados.core.logging import get_logger

log = get_logger("video.air_pipeline.auto_fallback")

# Where the runtime override lives. /run is tmpfs so a reboot clears
# the override — a transient bus_errors spike doesn't permanently
# disable hardware encoding.
OVERRIDE_PATH = Path("/run/ados/video-encoder-override.yaml")

# Default thresholds. Tuned to be quiet on a healthy pipeline and
# loud on a persistently broken one. A healthy mpph264enc pipeline
# emits 0 bus errors per minute. A broken one tends to emit one per
# encoded frame (30 fps), so 20 in 60 s is "obviously broken but not
# a single transient hiccup".
BUS_ERROR_THRESHOLD = 20
BUS_ERROR_WINDOW_S = 60.0


def read_override() -> dict[str, Any] | None:
    """Read the runtime override file. Returns None if absent or
    unreadable; otherwise the parsed YAML dict."""
    try:
        text = OVERRIDE_PATH.read_text()
    except (FileNotFoundError, PermissionError, OSError):
        return None
    try:
        loaded = yaml.safe_load(text)
    except yaml.YAMLError:
        return None
    if not isinstance(loaded, dict):
        return None
    return loaded


def write_auto_fallback_override(reason: str) -> None:
    """Persist the auto-fallback decision so subsequent VideoConfig
    loads see ``use_gst_air_pipeline: false``.

    Writes a small YAML document with the reason recorded so the
    operator running ``cat /run/ados/video-encoder-override.yaml``
    sees both the effective flag and why it was set. Silent on
    failure (the directory may not exist on a stripped rootfs; we
    still want the in-process fallback to fire even if persistence
    is impossible).
    """
    payload = {
        "use_gst_air_pipeline": False,
        "reason": reason,
        "written_at": time.time(),
    }
    try:
        OVERRIDE_PATH.parent.mkdir(parents=True, exist_ok=True)
        OVERRIDE_PATH.write_text(yaml.safe_dump(payload, sort_keys=True))
    except (PermissionError, OSError) as exc:
        log.warning(
            "video_encoder_override_write_failed",
            path=str(OVERRIDE_PATH),
            error=str(exc),
        )


def clear_auto_fallback_override() -> None:
    """Remove the override file. Used by operator-facing tooling that
    wants to re-attempt the hardware path after fixing the root cause
    (e.g. installed the right gstreamer-rockchip plugin). Silent on
    missing file."""
    try:
        OVERRIDE_PATH.unlink()
    except FileNotFoundError:
        return
    except (PermissionError, OSError) as exc:
        log.warning(
            "video_encoder_override_clear_failed",
            path=str(OVERRIDE_PATH),
            error=str(exc),
        )


def is_auto_fallback_active() -> bool:
    """True if the override file currently disables the GStreamer
    path. Callers in the config-load hot path use this BEFORE running
    the per-board default factory; it gives the self-heal precedence
    over the board default but never overrides an explicit operator
    value (explicit values bypass the factory entirely)."""
    payload = read_override()
    if payload is None:
        return False
    flag = payload.get("use_gst_air_pipeline")
    return flag is False


class AirPipelineHealthWatcher:
    """Sliding-window watcher over the AirPipeline's bus_errors counter.

    Feed it a (timestamp, cumulative bus_errors) snapshot on each
    stats publish tick. It tracks the rate over a rolling window and
    fires ``maybe_trigger_fallback()`` when the count of NEW errors
    inside the window exceeds the configured threshold.

    The watcher is stateful (keeps a deque of observations) but
    side-effect free until ``maybe_trigger_fallback()`` is called, so
    a test harness can inspect ``would_trigger()`` to assert the
    decision without writing files.

    Calling ``maybe_trigger_fallback()`` is idempotent: once the
    override file is in place we don't repeat the write.
    """

    def __init__(
        self,
        threshold: int = BUS_ERROR_THRESHOLD,
        window_s: float = BUS_ERROR_WINDOW_S,
    ) -> None:
        self._threshold = threshold
        self._window_s = window_s
        # (monotonic_seconds, cumulative_bus_errors) snapshots.
        self._samples: deque[tuple[float, int]] = deque()
        self._already_triggered = False

    def observe(self, bus_errors: int, now_s: float | None = None) -> None:
        """Record one snapshot. Evicts samples older than the window."""
        if now_s is None:
            now_s = time.monotonic()
        self._samples.append((now_s, int(bus_errors)))
        cutoff = now_s - self._window_s
        while self._samples and self._samples[0][0] < cutoff:
            self._samples.popleft()

    def would_trigger(self) -> bool:
        """Return True if the deque shows >= threshold NEW errors
        inside the window. Reads the deque without mutating it."""
        if len(self._samples) < 2:
            return False
        first_count = self._samples[0][1]
        last_count = self._samples[-1][1]
        return (last_count - first_count) >= self._threshold

    def maybe_trigger_fallback(self) -> bool:
        """If the watcher's window exceeds threshold AND we haven't
        already fired in this process lifetime, persist the override
        and emit a structured log. Returns True iff the fallback was
        newly triggered.

        Idempotent: subsequent calls inside the same process do
        nothing once the override is in place. A process restart
        re-evaluates from scratch (so a fresh agent boot with a still-
        broken pipeline will re-trigger and re-persist)."""
        if self._already_triggered:
            return False
        if not self.would_trigger():
            return False
        self._already_triggered = True
        first_count = self._samples[0][1]
        last_count = self._samples[-1][1]
        delta = last_count - first_count
        reason = (
            f"air_pipeline bus_errors increased by {delta} in "
            f"{self._window_s:.0f}s window (threshold {self._threshold})"
        )
        log.warning(
            "video_encoder_auto_fallback",
            bus_errors_delta=delta,
            window_s=self._window_s,
            threshold=self._threshold,
        )
        write_auto_fallback_override(reason)
        return True
