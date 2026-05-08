"""Touchscreen affine transform: raw ADC -> LCD pixel coordinates.

The ADS7846 driver reports raw 12-bit ADC counts in 0..4095. The LCD
panel may be rotated 0/90/180/270 degrees, and the resistive touch
overlay is rarely perfectly aligned with the visible pixels. Both
problems collapse into a single 2x3 affine matrix that maps raw
``(x_raw, y_raw)`` to LCD ``(x_px, y_px)``.

This module exposes:

* :class:`Affine` — the matrix dataclass with apply/save/load.
* :func:`identity_for` — sensible default mapping for a board whose
  calibration file has not been written yet (raw 0..4095 -> LCD
  bounds with the configured rotation).
* :func:`compute_from_samples` — least-squares fit of 5+ sample
  pairs, returns the matrix and the RMS residual in LCD pixels.
* :func:`load` / :func:`save` / :func:`save_skip_marker` —
  persistence helpers around ``/etc/ados/touch.calib``.

The implementation uses pure Python normal equations. Five
sample/target pairs produce 10 equations in 6 unknowns, which is
massively over-determined; the closed-form least-squares solution is
trivial to compute without numpy and avoids dragging a 50 MB binary
dependency into the agent footprint.
"""

from __future__ import annotations

import json
import os
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path

# Raw ADC range exposed by the ADS7846 driver. Used by the identity
# transform. The driver emits values clipped to this range, so the
# math doesn't have to defend against out-of-range inputs.
_RAW_MIN = 0
_RAW_MAX = 4095

# Persistence schema version. Bumped only when the on-disk shape
# changes — readers fall back to the identity transform on a version
# mismatch rather than blindly trusting an unknown layout.
_CALIB_FILE_VERSION = 1


@dataclass(frozen=True)
class Affine:
    """A 2x3 affine matrix mapping raw ADC -> LCD pixels.

    The transform is::

        x_lcd = a * x_raw + b * y_raw + c
        y_lcd = d * x_raw + e * y_raw + f

    The dataclass is frozen so persisted matrices stay immutable
    inside the bridge while it is reading events.
    """

    a: float
    b: float
    c: float
    d: float
    e: float
    f: float

    def apply(self, x_raw: int, y_raw: int) -> tuple[int, int]:
        """Map a raw sample to LCD pixel coordinates."""
        x = self.a * x_raw + self.b * y_raw + self.c
        y = self.d * x_raw + self.e * y_raw + self.f
        return int(round(x)), int(round(y))

    def to_list(self) -> list[float]:
        """Flat 6-float list for JSON persistence."""
        return [self.a, self.b, self.c, self.d, self.e, self.f]

    @classmethod
    def from_list(cls, values: list[float] | tuple[float, ...]) -> Affine:
        if len(values) != 6:
            raise ValueError(
                f"affine requires 6 values, got {len(values)}"
            )
        a, b, c, d, e, f = (float(v) for v in values)
        return cls(a=a, b=b, c=c, d=d, e=e, f=f)


def identity_for(rotation: int, lcd_size: tuple[int, int]) -> Affine:
    """Return a baseline affine that maps raw ADC -> LCD with rotation.

    With rotation=0 a raw 0..4095 sweep along x corresponds to LCD
    0..lcd_w along x; the raw 0..4095 sweep along y corresponds to
    LCD 0..lcd_h along y. Rotations apply in 90-degree steps.

    The returned matrix is the best-effort fallback when no
    calibration file exists; it is not perfectly accurate (the touch
    panel is never exactly square with the LCD) but is correct enough
    to dispatch tab-bar taps, which is what matters before the
    operator runs the wizard.
    """
    rotation = rotation % 360
    if rotation not in (0, 90, 180, 270):
        rotation = 0
    lcd_w, lcd_h = lcd_size
    span = float(_RAW_MAX - _RAW_MIN)
    if span <= 0:
        span = 1.0
    sx = lcd_w / span
    sy = lcd_h / span
    if rotation == 0:
        # x_lcd = sx * x_raw, y_lcd = sy * y_raw
        return Affine(a=sx, b=0.0, c=0.0, d=0.0, e=sy, f=0.0)
    if rotation == 90:
        # 90 deg clockwise: x_lcd = sx * y_raw,
        # y_lcd = sy * (max - x_raw)
        return Affine(
            a=0.0,
            b=lcd_w / span,
            c=0.0,
            d=-lcd_h / span,
            e=0.0,
            f=lcd_h,
        )
    if rotation == 180:
        return Affine(
            a=-sx, b=0.0, c=float(lcd_w),
            d=0.0, e=-sy, f=float(lcd_h),
        )
    # 270
    return Affine(
        a=0.0,
        b=-lcd_w / span,
        c=float(lcd_w),
        d=lcd_h / span,
        e=0.0,
        f=0.0,
    )


def compute_from_samples(
    samples: list[tuple[int, int]],
    targets: list[tuple[int, int]],
) -> tuple[Affine, float]:
    """Fit an affine matrix to 5+ raw/target pairs by least squares.

    Returns ``(affine, rms_residual_px)`` where ``rms_residual_px`` is
    the root-mean-square distance between the LCD-projected sample
    and the target, in LCD pixels. RMS under ~5 px on a clean panel,
    50 px is the rejection threshold the wizard uses.

    Raises ``ValueError`` if fewer than 5 pairs are supplied or if
    the system is degenerate (all samples in a line).
    """
    if len(samples) < 5 or len(targets) < 5:
        raise ValueError("need at least 5 sample/target pairs")
    if len(samples) != len(targets):
        raise ValueError("samples and targets must be the same length")

    # The affine has six independent parameters because the x and y
    # equations decouple: the x coefficients (a, b, c) are fit using
    # only sample x_raw/y_raw and target x; the y coefficients
    # (d, e, f) are fit using the same raw inputs and target y.
    # We assemble the 3x3 normal-equations matrix once and solve it
    # twice (right-hand-side x_target, then y_target).
    sxx = sxy = sx = syy = sy = n = 0.0
    txx = txy = tx_ = 0.0  # for x targets
    tyx = tyy = ty_ = 0.0  # for y targets
    for (x_raw, y_raw), (x_t, y_t) in zip(samples, targets):
        x_raw_f = float(x_raw)
        y_raw_f = float(y_raw)
        sxx += x_raw_f * x_raw_f
        sxy += x_raw_f * y_raw_f
        syy += y_raw_f * y_raw_f
        sx += x_raw_f
        sy += y_raw_f
        n += 1.0
        txx += x_raw_f * x_t
        txy += y_raw_f * x_t
        tx_ += x_t
        tyx += x_raw_f * y_t
        tyy += y_raw_f * y_t
        ty_ += y_t

    # Normal-equations matrix M (3x3, symmetric):
    #   [[sxx, sxy, sx],
    #    [sxy, syy, sy],
    #    [sx,  sy,  n ]]
    m = [
        [sxx, sxy, sx],
        [sxy, syy, sy],
        [sx, sy, n],
    ]
    abc = _solve3(m, [txx, txy, tx_])
    def_ = _solve3(m, [tyx, tyy, ty_])
    affine = Affine(
        a=abc[0], b=abc[1], c=abc[2],
        d=def_[0], e=def_[1], f=def_[2],
    )

    # RMS residual computed by re-applying the matrix to every sample.
    sqsum = 0.0
    for (x_raw, y_raw), (x_t, y_t) in zip(samples, targets):
        x_p, y_p = affine.apply(x_raw, y_raw)
        dx = x_p - x_t
        dy = y_p - y_t
        sqsum += dx * dx + dy * dy
    rms = (sqsum / max(1, len(samples))) ** 0.5
    return affine, rms


def _solve3(m: list[list[float]], rhs: list[float]) -> list[float]:
    """Solve a 3x3 linear system via Gaussian elimination with partial pivoting.

    Returns the solution vector. Raises ``ValueError`` if the system
    is singular (e.g. all samples lie on a line, which makes the
    normal-equations matrix rank-deficient).
    """
    # Augmented matrix.
    a = [row[:] + [b] for row, b in zip(m, rhs)]
    n = 3
    for i in range(n):
        # Partial pivot — pick the row with max |a[k][i]| on or below
        # the diagonal.
        pivot = i
        max_abs = abs(a[i][i])
        for k in range(i + 1, n):
            if abs(a[k][i]) > max_abs:
                max_abs = abs(a[k][i])
                pivot = k
        if max_abs < 1e-12:
            raise ValueError("affine fit is singular (samples colinear)")
        if pivot != i:
            a[i], a[pivot] = a[pivot], a[i]
        # Eliminate.
        for k in range(i + 1, n):
            factor = a[k][i] / a[i][i]
            for j in range(i, n + 1):
                a[k][j] -= factor * a[i][j]
    # Back-substitute.
    x = [0.0] * n
    for i in range(n - 1, -1, -1):
        s = a[i][n]
        for j in range(i + 1, n):
            s -= a[i][j] * x[j]
        x[i] = s / a[i][i]
    return x


def load(path: Path) -> Affine | None:
    """Read the persisted affine from ``path``.

    Returns None when the file is missing, malformed, marked
    ``calibrated=False`` (a skip marker), or the version field doesn't
    match this build. The bridge falls back to :func:`identity_for`
    in any of those cases.
    """
    try:
        text = path.read_text()
    except OSError:
        return None
    try:
        data = json.loads(text)
    except json.JSONDecodeError:
        return None
    if not isinstance(data, dict):
        return None
    if data.get("version") != _CALIB_FILE_VERSION:
        return None
    if not data.get("calibrated"):
        return None
    matrix = data.get("matrix")
    if not isinstance(matrix, list) or len(matrix) != 6:
        return None
    try:
        return Affine.from_list(matrix)
    except (ValueError, TypeError):
        return None


def save(
    affine: Affine,
    path: Path,
    *,
    rotation: int,
    rms: float,
    device: str = "ads7846",
    raw_range: tuple[int, int] = (_RAW_MIN, _RAW_MAX),
    lcd_size: tuple[int, int] = (480, 320),
) -> None:
    """Persist the affine atomically to ``path``.

    The blob captures version, the device hint, raw ADC range, the
    LCD geometry the calibration was taken at, the rotation that was
    applied, the matrix itself, the RMS residual, and a UNIX
    timestamp. Atomic write via tmpfile + rename so a power loss
    mid-save can never half-write the file.
    """
    blob = {
        "version": _CALIB_FILE_VERSION,
        "calibrated": True,
        "device": device,
        "raw_range": [int(raw_range[0]), int(raw_range[1])],
        "lcd_size": [int(lcd_size[0]), int(lcd_size[1])],
        "rotation_applied_at_save": int(rotation) % 360,
        "matrix": affine.to_list(),
        "rms_residual_px": float(rms),
        "saved_at": int(time.time()),
    }
    _atomic_write_json(path, blob)


def save_skip_marker(path: Path) -> None:
    """Persist a marker that says the operator chose to skip calibration.

    The bridge treats this the same as "no calibration file" and uses
    the identity transform — but the marker stops the wizard from
    auto-launching every boot.
    """
    blob = {
        "version": _CALIB_FILE_VERSION,
        "calibrated": False,
        "skipped": True,
        "saved_at": int(time.time()),
    }
    _atomic_write_json(path, blob)


def _atomic_write_json(path: Path, blob: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp_path = tempfile.mkstemp(
        prefix=path.name + ".",
        suffix=".tmp",
        dir=str(path.parent),
    )
    try:
        with os.fdopen(fd, "w") as fh:
            json.dump(blob, fh, separators=(",", ":"))
            fh.flush()
            os.fsync(fh.fileno())
        os.replace(tmp_path, path)
    except Exception:
        try:
            os.unlink(tmp_path)
        except OSError:
            pass
        raise
