"""libinput calibration matrix for an HDMI display's SPI resistive touch.

An HDMI display can carry a standalone XPT2046 / ADS7846 SPI resistive-touch
layer: the video arrives over HDMI (the kernel DRM driver owns the
framebuffer) while the touch rides SPI as a plain libinput touchscreen. cage
(the kiosk's Wayland compositor) consumes that touchscreen through libinput,
which applies a per-device ``LIBINPUT_CALIBRATION_MATRIX`` udev property to
map the resistive contact onto the output.

This module owns the math for that matrix and the udev rule that carries it.
The overlay installer writes an initial rule from the board's declared touch
bounds; the on-screen calibration wizard (``/api/display/calibrate/*``) refits
the touch against on-screen targets and calls :func:`regenerate_from_calibration`
to rewrite the same rule from the fresh fit — so the existing calibration
mechanism re-calibrates the HDMI touch device, not just an SPI-LCD panel.

The matrix is a 6-value 2x3 affine ``[a b c d e f]`` in libinput's convention::

    out_x = a * n_x + b * n_y + c
    out_y = d * n_x + e * n_y + f

where ``n_x``/``n_y`` are the device-normalized coordinates libinput derives
from the driver's ABS range (the touch overlay leaves the driver at its
default 0..4095 12-bit range, so ``n = raw / 4095``) and ``out_x``/``out_y``
are normalized 0..1 output coordinates. cage then scales the normalized output
onto whatever resolution the HDMI mode is running at.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import tempfile
from pathlib import Path

from ados.core.logging import get_logger
from ados.core.paths import (
    DISPLAY_CONF_PATH,
    HDMI_TOUCH_UDEV_RULE_PATH,
    TOUCH_CALIB_PATH,
)
from ados.services.ui.touch.transform import Affine
from ados.services.ui.touch.transform import load as load_calib

log = get_logger("ui.touch.libinput")

# Full-scale of the ADS7846/XPT2046 12-bit ADC. The touch overlay leaves the
# driver's default ABS range (0..MAX_12BIT), so libinput normalizes raw counts
# over this span before applying the matrix.
ADC_MAX = 4095

# The kernel input device name the ads7846 driver reports. The udev rule keys
# on it so only the touch device gets the calibration matrix.
DEFAULT_TOUCH_DEVICE_NAME = "ADS7846 Touchscreen"

Matrix = tuple[float, float, float, float, float, float]


def matrix_from_bounds(
    x_min: int,
    x_max: int,
    y_min: int,
    y_max: int,
    *,
    swap_xy: bool = False,
    invert_x: bool = False,
    invert_y: bool = False,
    adc_max: int = ADC_MAX,
) -> Matrix:
    """Compute a libinput matrix from raw ADC edge bounds + orientation.

    ``x_min``/``x_max`` are the raw ADC counts read at the left/right physical
    edges of the touch area, ``y_min``/``y_max`` at the top/bottom edges. A
    touch at ``x_min`` maps to output 0, ``x_max`` to output 1 (before any
    invert). ``swap_xy`` exchanges the touch axes onto the screen axes;
    ``invert_x``/``invert_y`` flip the resulting screen axis. This is the
    best-guess baseline the overlay installer ships; the wizard refits it via
    :func:`matrix_from_affine`.

    Degenerate bounds (``max <= min``) fall back to the full ADC span for that
    axis so the result can never divide by zero.
    """
    span_x = float(x_max - x_min)
    span_y = float(y_max - y_min)
    if span_x <= 0:
        x_min, span_x = 0, float(adc_max)
    if span_y <= 0:
        y_min, span_y = 0, float(adc_max)

    # Per-axis: out = a_axis * n + c_axis maps raw x_min -> 0, x_max -> 1,
    # given n = raw / adc_max, so raw = n * adc_max.
    a_x = adc_max / span_x
    c_x = -x_min / span_x
    a_y = adc_max / span_y
    c_y = -y_min / span_y

    def flip(scale: float, offset: float, invert: bool) -> tuple[float, float]:
        # out -> 1 - out inverts the axis about the output centre.
        return (-scale, 1.0 - offset) if invert else (scale, offset)

    sx, cx = flip(a_x, c_x, invert_x)
    sy, cy = flip(a_y, c_y, invert_y)

    if swap_xy:
        # Screen X is driven by the touch Y axis and vice versa.
        return (0.0, sy, cy, sx, 0.0, cx)
    return (sx, 0.0, cx, 0.0, sy, cy)


def matrix_from_affine(
    affine: Affine,
    output_w: int,
    output_h: int,
    *,
    adc_max: int = ADC_MAX,
) -> Matrix:
    """Convert a fit raw-ADC -> output-pixel affine into a libinput matrix.

    The calibration wizard fits an :class:`Affine` that maps raw ADC counts
    directly to output pixels in ``[0, output_w] x [0, output_h]``. libinput
    instead wants device-normalized input (``n = raw / adc_max``) mapped to
    normalized 0..1 output. Substituting ``raw = n * adc_max`` and dividing the
    pixel result by the output size yields the libinput coefficients:

        out_norm_x = (a*raw_x + b*raw_y + c) / W
                   = (a*adc_max/W) n_x + (b*adc_max/W) n_y + (c/W)

    ``output_w``/``output_h`` are the pixel geometry the wizard's targets were
    laid out at (recorded in the saved calibration blob), so the mapping is
    correct for whatever resolution the wizard ran at.
    """
    w = float(output_w) or 1.0
    h = float(output_h) or 1.0
    return (
        affine.a * adc_max / w,
        affine.b * adc_max / w,
        affine.c / w,
        affine.d * adc_max / h,
        affine.e * adc_max / h,
        affine.f / h,
    )


def format_matrix(matrix: Matrix) -> str:
    """Format the 6-value matrix as a space-separated string for udev."""
    return " ".join(f"{v:.6g}" for v in matrix)


def udev_rule_text(matrix: Matrix, *, device_name: str = DEFAULT_TOUCH_DEVICE_NAME) -> str:
    """Render the udev rule that assigns LIBINPUT_CALIBRATION_MATRIX.

    Guarded with a subsystem GOTO so the ``ATTRS{name}`` match only runs for
    input devices, matching the form libinput documents for touch calibration.
    """
    return (
        "# Written by the ADOS display-overlay installer / calibration wizard.\n"
        "# Maps an HDMI display's XPT2046/ADS7846 resistive touch onto the output\n"
        "# for cage/libinput. Regenerated when the touch is recalibrated on the rig.\n"
        'SUBSYSTEM!="input", GOTO="ados_hdmi_touch_end"\n'
        f'ATTRS{{name}}=="{device_name}", '
        f'ENV{{LIBINPUT_CALIBRATION_MATRIX}}="{format_matrix(matrix)}"\n'
        'LABEL="ados_hdmi_touch_end"\n'
    )


def write_udev_rule(
    matrix: Matrix,
    *,
    device_name: str = DEFAULT_TOUCH_DEVICE_NAME,
    path: Path = HDMI_TOUCH_UDEV_RULE_PATH,
    reload_udev: bool = True,
) -> None:
    """Write the udev calibration rule atomically and reload udev.

    The reload is best-effort: the touch input device only appears after the
    SPI overlay is loaded (a reboot), so the reload just makes a live device
    pick up a refit without waiting for the next boot.
    """
    text = udev_rule_text(matrix, device_name=device_name)
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp = tempfile.mkstemp(prefix=path.name + ".", suffix=".tmp", dir=str(path.parent))
    try:
        with os.fdopen(fd, "w") as fh:
            fh.write(text)
            fh.flush()
            os.fsync(fh.fileno())
        os.replace(tmp, path)
        path.chmod(0o644)
    except Exception:
        try:
            os.unlink(tmp)
        except OSError:
            pass
        raise
    if reload_udev:
        _reload_udev()


def _reload_udev() -> None:
    """Best-effort ``udevadm control --reload-rules`` + input re-trigger."""
    udevadm = shutil.which("udevadm")
    if not udevadm:
        return
    for args in (
        [udevadm, "control", "--reload-rules"],
        [udevadm, "trigger", "--subsystem-match=input"],
    ):
        try:
            subprocess.run(args, check=False, capture_output=True, timeout=10)
        except (OSError, subprocess.SubprocessError) as exc:
            log.debug("udevadm_reload_failed", args=args, error=str(exc))


def parse_display_conf(path: Path = DISPLAY_CONF_PATH) -> dict[str, str]:
    """Read the ``key=value`` display.conf into a dict (missing file -> {})."""
    out: dict[str, str] = {}
    try:
        text = path.read_text()
    except OSError:
        return out
    for raw in text.splitlines():
        line = raw.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        k, _, v = line.partition("=")
        out[k.strip()] = v.strip()
    return out


def _saved_output_size(calib_path: Path) -> tuple[int, int] | None:
    """Read the ``lcd_size`` the calibration was fit at from the saved blob.

    The wizard records the pixel geometry its targets were laid out at, which
    is the output geometry :func:`matrix_from_affine` must normalize against.
    """
    import json

    try:
        blob = json.loads(calib_path.read_text())
    except (OSError, json.JSONDecodeError):
        return None
    size = blob.get("lcd_size")
    if isinstance(size, list) and len(size) == 2:
        try:
            return int(size[0]), int(size[1])
        except (TypeError, ValueError):
            return None
    return None


def regenerate_from_calibration(
    *,
    display_conf_path: Path = DISPLAY_CONF_PATH,
    calib_path: Path = TOUCH_CALIB_PATH,
    udev_rule_path: Path = HDMI_TOUCH_UDEV_RULE_PATH,
    reload_udev: bool = True,
) -> bool:
    """Rewrite the udev calibration rule from the wizard's fresh fit.

    A no-op that returns ``False`` unless the active display is an
    ``hdmi-touch`` one AND a real calibration (not a skip marker) is on disk.
    Called by ``/api/display/calibrate/save`` so recalibrating the touch on an
    HDMI kiosk updates the libinput matrix cage reads, the same way it updates
    the SPI-LCD affine on a framebuffer panel.
    """
    conf = parse_display_conf(display_conf_path)
    if conf.get("type") != "hdmi-touch":
        return False
    affine = load_calib(calib_path)
    if affine is None:
        return False

    output_w, output_h = _saved_output_size(calib_path) or _resolution_from_conf(conf)
    matrix = matrix_from_affine(affine, output_w, output_h)
    device_name = conf.get("touch_device_name") or DEFAULT_TOUCH_DEVICE_NAME
    write_udev_rule(
        matrix,
        device_name=device_name,
        path=udev_rule_path,
        reload_udev=reload_udev,
    )
    log.info(
        "hdmi_touch_calibration_regenerated",
        matrix=format_matrix(matrix),
        output=f"{output_w}x{output_h}",
        device=device_name,
    )
    return True


def _resolution_from_conf(conf: dict[str, str]) -> tuple[int, int]:
    """Parse ``resolution=800x480`` from display.conf, defaulting to 800x480."""
    res = conf.get("resolution", "")
    if "x" in res:
        w, _, h = res.partition("x")
        try:
            return int(w), int(h)
        except ValueError:
            pass
    return 800, 480
