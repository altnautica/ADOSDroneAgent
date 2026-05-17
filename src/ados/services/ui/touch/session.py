"""Process-wide calibration session, shared by REST and the on-LCD wizard.

The 9-point calibration flow has two co-equal entry points:

* The on-LCD wizard rendered by ``CalibrationWizard`` and driven by the
  OLED service when the operator taps the panel.
* The Mission Control GCS, which calls ``/api/v1/display/calibrate/*``
  routes from a remote browser to drive the same flow without the
  operator standing in front of the panel.

Both must share state. If a remote ``/start`` request comes in, the
panel must enter wizard mode immediately so the operator on the bench
sees the targets. If the operator taps target #2 on the panel, the
remote ``/status`` poll must reflect ``current_step=2``. The simplest
way to deliver that is a process-singleton ``CalibrationSession`` that
both halves read and mutate.

The session itself is pure state; the on-LCD renderer and the bridge
calibration handler still own their existing state machines, but they
mirror their step / sample / completion transitions into this object
so the REST surface can introspect them. The REST surface also pushes
into the same object — when ``/start`` flips ``in_progress=True`` the
OLED service watches the field on every tick and engages the wizard.
"""

from __future__ import annotations

import threading
import time
from dataclasses import dataclass, field
from pathlib import Path

from ados.core.paths import TOUCH_CALIB_PATH

from .transform import compute_from_samples
from .transform import save as save_calib
from .transform import save_skip_marker as save_skip_calib

# Targets mirror the on-LCD wizard. Centralised here so the REST surface
# and the on-LCD wizard never drift on the geometry. (The wizard module
# imports nothing from here to keep the import graph acyclic; an update
# to one constant must be mirrored to the other in lock step.)
# 3x3 grid: 9 targets give an over-determined fit (18 equations in 6
# unknowns), reducing the per-tap noise floor ~1.5x vs a 5-point fit.
TARGETS: tuple[tuple[int, int], ...] = (
    (40, 40),       # top-left
    (240, 40),      # top-center
    (440, 40),      # top-right
    (40, 160),      # left-center
    (240, 160),     # center
    (440, 160),     # right-center
    (40, 280),      # bottom-left
    (240, 280),     # bottom-center
    (440, 280),     # bottom-right
)

STEP_COUNT = len(TARGETS)
# 9-point over-determined fit (18 equations, 6 unknowns) reduces the
# per-tap noise floor ~1.5x vs the older 5-point pattern, so the RMS
# rejection threshold is tightened from 50 px to 35 px.
REJECT_RMS_PX = 35.0


@dataclass
class CalibrationSession:
    """Mutable state shared by the on-LCD wizard and the REST routes.

    ``current_step`` is the next sample index the wizard expects. ``0``
    means "tap target #1", ``STEP_COUNT`` means "all samples in, ready
    to save". ``in_progress`` is True between ``start()`` and a terminal
    ``save()`` / ``skip()`` call. ``samples`` carries the raw ADC
    coordinates supplied so far.
    """

    in_progress: bool = False
    current_step: int = 0
    samples: list[tuple[int, int]] = field(default_factory=list)
    rms_residual_px: float | None = None
    started_at: float | None = None
    save_path: Path = TOUCH_CALIB_PATH

    # Generation counter. Bumped on start() so the on-LCD service can
    # detect that the REST surface armed a new wizard run since the
    # last tick and engage calibrate mode without polling start_at.
    generation: int = 0


class _SessionRegistry:
    """Thread-safe wrapper around the singleton session.

    The on-LCD wizard runs on the asyncio loop in the OLED service; the
    REST surface runs on the FastAPI thread; both reach into the same
    object. A bare lock around mutators is enough — no concurrent
    write contention is expected, and reads are short.
    """

    def __init__(self) -> None:
        self._session = CalibrationSession()
        self._lock = threading.Lock()

    def snapshot(self) -> CalibrationSession:
        """Return a shallow copy of the current state."""
        with self._lock:
            s = self._session
            return CalibrationSession(
                in_progress=s.in_progress,
                current_step=s.current_step,
                samples=list(s.samples),
                rms_residual_px=s.rms_residual_px,
                started_at=s.started_at,
                save_path=s.save_path,
                generation=s.generation,
            )

    def start(self, save_path: Path | None = None) -> CalibrationSession:
        """Reset the session and arm a new wizard run.

        Returns the snapshot the REST handler ships back to the caller.
        Bumps the generation counter so the OLED service can engage
        calibrate mode on the next render tick.
        """
        with self._lock:
            self._session = CalibrationSession(
                in_progress=True,
                current_step=0,
                samples=[],
                rms_residual_px=None,
                started_at=time.monotonic(),
                save_path=save_path or TOUCH_CALIB_PATH,
                generation=self._session.generation + 1,
            )
            return CalibrationSession(
                in_progress=True,
                current_step=0,
                samples=[],
                rms_residual_px=None,
                started_at=self._session.started_at,
                save_path=self._session.save_path,
                generation=self._session.generation,
            )

    def submit_sample(
        self, step: int, x_raw: int, y_raw: int,
    ) -> tuple[bool, int, bool]:
        """Record a sample if the step matches the next-expected one.

        Returns ``(accepted, next_step, complete)``.

        * ``accepted`` is True when the step matched and the sample
          was appended.
        * ``next_step`` is the new ``current_step`` after the append
          (or unchanged when rejected).
        * ``complete`` is True when all ``STEP_COUNT`` samples are in.
        """
        with self._lock:
            s = self._session
            if not s.in_progress:
                return False, s.current_step, False
            if step != s.current_step:
                return False, s.current_step, s.current_step >= STEP_COUNT
            s.samples.append((int(x_raw), int(y_raw)))
            s.current_step += 1
            return True, s.current_step, s.current_step >= STEP_COUNT

    def save(self, *, rotation: int = 0) -> tuple[bool, float | None, str | None]:
        """Solve the affine + persist if RMS is acceptable.

        Returns ``(ok, rms_residual_px, error)``. On a clean fit the
        affine is written to ``save_path`` and ``in_progress`` flips to
        False. On rejection (RMS over threshold or insufficient
        samples) the file is left alone so the next start() can retry
        without the bridge falling through to a bad transform.
        """
        with self._lock:
            s = self._session
            if not s.in_progress:
                return False, None, "not_in_progress"
            if len(s.samples) < STEP_COUNT:
                return False, None, "incomplete"
            try:
                affine, rms = compute_from_samples(
                    list(s.samples), list(TARGETS),
                )
            except ValueError as exc:
                s.rms_residual_px = None
                return False, None, str(exc)
            if rms > REJECT_RMS_PX:
                s.rms_residual_px = float(rms)
                return False, float(rms), "rms_above_threshold"
            try:
                save_calib(
                    affine,
                    s.save_path,
                    rotation=rotation,
                    rms=float(rms),
                    lcd_size=(480, 320),
                )
            except OSError as exc:
                return False, float(rms), f"persist_failed: {exc}"
            s.rms_residual_px = float(rms)
            s.in_progress = False
            return True, float(rms), None

    def skip(self) -> bool:
        """Persist a skip marker and end the session.

        Returns True on success, False on persistence error. The skip
        marker has the same on-disk shape as a "no calibration" file
        but suppresses the wizard's auto-launch on next boot.
        """
        with self._lock:
            s = self._session
            try:
                save_skip_calib(s.save_path)
            except OSError:
                return False
            s.in_progress = False
            s.current_step = 0
            s.samples = []
            s.rms_residual_px = None
            return True

    def mirror_step(self, step: int, samples: list[tuple[int, int]]) -> None:
        """Mirror the on-LCD wizard's progression into the shared session.

        Called from the OLED service every time it accepts a sample on
        the panel so a remote ``/status`` poll sees the live step
        counter without racing the wizard's private state.
        """
        with self._lock:
            s = self._session
            s.in_progress = True
            s.current_step = max(s.current_step, int(step))
            s.samples = list(samples)

    def mirror_complete(self, *, rms_residual_px: float, success: bool) -> None:
        """Mirror the on-LCD wizard's terminal state."""
        with self._lock:
            s = self._session
            s.rms_residual_px = float(rms_residual_px)
            s.in_progress = False if success else s.in_progress

    def reset(self) -> None:
        """Clear the session entirely. Used by tests + agent shutdown."""
        with self._lock:
            self._session = CalibrationSession()


_REGISTRY = _SessionRegistry()


def get_session_registry() -> _SessionRegistry:
    """Return the process-wide session registry."""
    return _REGISTRY
