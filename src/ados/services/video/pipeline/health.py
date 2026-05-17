"""Health-tick bookkeeping for :class:`VideoPipeline`.

Encapsulates the helpers that drive the consecutive-healthy
sliding window: the public ``restart_attempts()`` surface used by
the cloud heartbeat, ``_note_healthy_tick()`` (which clears the
restart counter after a sustained run of healthy probes), and
``_note_unhealthy_tick()`` (which arms the window on a failed
probe).

The orchestrator's main ``_check_health()`` and ``run()`` loop
stay over in ``pipeline.py`` — startup-grace + first-packet
detection + restart-counter accounting are too intertwined with
the encoder + mediamtx lifecycle to move without risk. Only the
two-line stamping helpers are here.

The mixin holds methods only — every attribute the methods touch
is declared in :class:`VideoPipeline.__init__` over in
``pipeline.py``.
"""

from __future__ import annotations

import time


class _HealthMixin:
    """Restart-counter bookkeeping grafted onto :class:`VideoPipeline`."""

    def restart_attempts(self) -> int:
        """Public accessor for the encoder restart counter.

        Surfaced on the cloud heartbeat so the GCS health view can
        flag a flapping pipeline before the circuit breaker fires.
        """
        return self._restart_count

    def _note_healthy_tick(self, now: float | None = None) -> bool:
        """Stamp a healthy probe and clear the counter when stable.

        Returns True if the restart counter was just cleared as a
        result of this call. Carved out of `run()` so the reset
        decision can be tested without driving the infinite loop.
        """
        from .pipeline import _pkg

        log = _pkg().log
        if now is None:
            now = time.monotonic()
        if self._last_healthy_at == 0.0:
            self._last_healthy_at = now
            return False
        if (
            self._restart_count > 0
            and now - self._last_healthy_at
            > self._healthy_reset_window_secs
        ):
            log.info(
                "pipeline_restart_counter_reset",
                msg="healthy window reached, clearing counter",
                window_secs=self._healthy_reset_window_secs,
                attempts=self._restart_count,
            )
            self._restart_count = 0
            return True
        return False

    def _note_unhealthy_tick(self) -> None:
        """Reset the consecutive-healthy timer on a failed probe."""
        self._last_healthy_at = 0.0
