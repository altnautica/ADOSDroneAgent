"""Tests for the HDMI-touch libinput calibration matrix + udev regeneration.

Covers the matrix math (bounds -> matrix, fit-affine -> matrix), the udev rule
rendering + atomic write, and the ``regenerate_from_calibration`` flow that the
calibration wizard calls after a refit. A cross-check test asserts the shell
``compute_libinput_matrix`` in the overlay installer produces the identical
matrix as the Python ``matrix_from_bounds`` for a spread of inputs, so the two
implementations of the same formula cannot drift.
"""

from __future__ import annotations

import json
import shutil
import subprocess
from pathlib import Path

import pytest

from ados.services.ui.touch.libinput_calibration import (
    ADC_MAX,
    DEFAULT_TOUCH_DEVICE_NAME,
    format_matrix,
    matrix_from_affine,
    matrix_from_bounds,
    parse_display_conf,
    regenerate_from_calibration,
    udev_rule_text,
    write_udev_rule,
)
from ados.services.ui.touch.transform import Affine

REPO_ROOT = Path(__file__).resolve().parents[1]
OVERLAY_SCRIPT = REPO_ROOT / "scripts" / "drivers" / "install-display-overlay.sh"


# ---------------------------------------------------------------------------
# matrix_from_bounds
# ---------------------------------------------------------------------------
class TestMatrixFromBounds:
    def test_full_range_identity(self):
        # Bounds spanning the full ADC range with neutral orientation is the
        # identity matrix: no scale, no offset.
        m = matrix_from_bounds(0, ADC_MAX, 0, ADC_MAX)
        assert m == pytest.approx((1.0, 0.0, 0.0, 0.0, 1.0, 0.0))

    def test_inset_bounds_scale_and_offset(self):
        # A touch that only spans 200..3900 maps its own min/max to the output
        # corners: out = raw/span - min/span.
        m = matrix_from_bounds(200, 3900, 200, 3900)
        span = 3900 - 200
        assert m[0] == pytest.approx(ADC_MAX / span)
        assert m[2] == pytest.approx(-200 / span)
        assert m[4] == pytest.approx(ADC_MAX / span)
        assert m[5] == pytest.approx(-200 / span)
        # A tap at the raw min lands at output 0, raw max at output 1.
        assert m[0] * (200 / ADC_MAX) + m[2] == pytest.approx(0.0)
        assert m[0] * (3900 / ADC_MAX) + m[2] == pytest.approx(1.0)

    def test_invert_x_flips_axis(self):
        m = matrix_from_bounds(0, ADC_MAX, 0, ADC_MAX, invert_x=True)
        # raw 0 -> output 1, raw max -> output 0.
        assert m[0] * 0 + m[2] == pytest.approx(1.0)
        assert m[0] * 1 + m[2] == pytest.approx(0.0)

    def test_swap_xy_routes_touch_y_to_screen_x(self):
        m = matrix_from_bounds(0, ADC_MAX, 0, ADC_MAX, swap_xy=True)
        # Screen X coefficient sits in the n_y column (b), screen Y in n_x (d).
        a, b, c, d, e, f = m
        assert a == pytest.approx(0.0)
        assert b == pytest.approx(1.0)
        assert d == pytest.approx(1.0)
        assert e == pytest.approx(0.0)

    def test_degenerate_bounds_do_not_divide_by_zero(self):
        m = matrix_from_bounds(500, 500, 0, ADC_MAX)
        # Falls back to the full span for the degenerate axis; finite result.
        assert all(abs(v) < 1e6 for v in m)


# ---------------------------------------------------------------------------
# matrix_from_affine
# ---------------------------------------------------------------------------
class TestMatrixFromAffine:
    def test_identity_pixel_affine(self):
        # An affine that maps raw 0..ADC_MAX straight onto 0..W / 0..H pixels
        # normalizes back to the identity libinput matrix.
        w, h = 800, 480
        affine = Affine(a=w / ADC_MAX, b=0, c=0, d=0, e=h / ADC_MAX, f=0)
        m = matrix_from_affine(affine, w, h)
        assert m == pytest.approx((1.0, 0.0, 0.0, 0.0, 1.0, 0.0))

    def test_offset_affine_normalizes_by_output(self):
        w, h = 800, 480
        # A pixel affine with a constant offset: c/W becomes the matrix offset.
        affine = Affine(a=w / ADC_MAX, b=0, c=80, d=0, e=h / ADC_MAX, f=48)
        m = matrix_from_affine(affine, w, h)
        assert m[2] == pytest.approx(80 / w)
        assert m[5] == pytest.approx(48 / h)


class TestFormatAndRule:
    def test_format_matrix(self):
        assert format_matrix((1.0, 0.0, 0.0, 0.0, 1.0, 0.0)) == "1 0 0 0 1 0"

    def test_udev_rule_text_targets_touch_device(self):
        rule = udev_rule_text((1.0, 0.0, 0.0, 0.0, 1.0, 0.0))
        assert 'SUBSYSTEM!="input", GOTO="ados_hdmi_touch_end"' in rule
        assert f'ATTRS{{name}}=="{DEFAULT_TOUCH_DEVICE_NAME}"' in rule
        assert 'ENV{LIBINPUT_CALIBRATION_MATRIX}="1 0 0 0 1 0"' in rule
        assert 'LABEL="ados_hdmi_touch_end"' in rule

    def test_write_udev_rule_atomic(self, tmp_path: Path):
        path = tmp_path / "99-ados-hdmi-touch.rules"
        write_udev_rule(
            (1.0, 0.0, 0.0, 0.0, 1.0, 0.0),
            path=path,
            reload_udev=False,
        )
        assert path.exists()
        assert "LIBINPUT_CALIBRATION_MATRIX" in path.read_text()
        # No stray temp files left behind.
        assert list(tmp_path.iterdir()) == [path]


# ---------------------------------------------------------------------------
# regenerate_from_calibration
# ---------------------------------------------------------------------------
class TestRegenerate:
    def _write_conf(self, path: Path, **kv: str) -> None:
        path.write_text("\n".join(f"{k}={v}" for k, v in kv.items()) + "\n")

    def _write_calib(self, path: Path, affine: Affine, lcd_size=(800, 480)) -> None:
        path.write_text(
            json.dumps(
                {
                    "version": 1,
                    "calibrated": True,
                    "matrix": affine.to_list(),
                    "lcd_size": list(lcd_size),
                }
            )
        )

    def test_noop_when_not_hdmi_touch(self, tmp_path: Path):
        conf = tmp_path / "display.conf"
        self._write_conf(conf, type="spi-lcd", display_id="waveshare35a")
        calib = tmp_path / "touch.calib"
        self._write_calib(calib, Affine(1, 0, 0, 0, 1, 0))
        rule = tmp_path / "rule.rules"
        assert (
            regenerate_from_calibration(
                display_conf_path=conf,
                calib_path=calib,
                udev_rule_path=rule,
                reload_udev=False,
            )
            is False
        )
        assert not rule.exists()

    def test_noop_when_no_calibration(self, tmp_path: Path):
        conf = tmp_path / "display.conf"
        self._write_conf(conf, type="hdmi-touch")
        rule = tmp_path / "rule.rules"
        assert (
            regenerate_from_calibration(
                display_conf_path=conf,
                calib_path=tmp_path / "absent.calib",
                udev_rule_path=rule,
                reload_udev=False,
            )
            is False
        )
        assert not rule.exists()

    def test_regenerates_matrix_from_fit(self, tmp_path: Path):
        conf = tmp_path / "display.conf"
        self._write_conf(
            conf,
            type="hdmi-touch",
            resolution="800x480",
            touch_device_name="ADS7846 Touchscreen",
        )
        calib = tmp_path / "touch.calib"
        w, h = 800, 480
        affine = Affine(a=w / ADC_MAX, b=0, c=0, d=0, e=h / ADC_MAX, f=0)
        self._write_calib(calib, affine, lcd_size=(w, h))
        rule = tmp_path / "rule.rules"
        ok = regenerate_from_calibration(
            display_conf_path=conf,
            calib_path=calib,
            udev_rule_path=rule,
            reload_udev=False,
        )
        assert ok is True
        text = rule.read_text()
        assert 'ENV{LIBINPUT_CALIBRATION_MATRIX}="1 0 0 0 1 0"' in text
        assert 'ATTRS{name}=="ADS7846 Touchscreen"' in text

    def test_parse_display_conf(self, tmp_path: Path):
        conf = tmp_path / "display.conf"
        conf.write_text("# comment\ntype=hdmi-touch\nresolution=800x480\n\nbad line\n")
        parsed = parse_display_conf(conf)
        assert parsed == {"type": "hdmi-touch", "resolution": "800x480"}


# ---------------------------------------------------------------------------
# Shell/Python cross-check — the two matrix implementations must agree
# ---------------------------------------------------------------------------
def _shell_matrix(x0, x1, y0, y1, swap, ix, iy) -> tuple[float, ...]:
    """Invoke the installer's compute_libinput_matrix and parse its 6 floats.

    Extracts just the function body from the installer so sourcing it does not
    run the whole installer (which parses args + exits early).
    """
    harness = _SHELL_HARNESS.replace("__SCRIPT__", str(OVERLAY_SCRIPT))
    out = subprocess.run(
        ["bash", "-c", harness, "--",
         str(x0), str(x1), str(y0), str(y1), str(swap), str(ix), str(iy)],
        capture_output=True,
        text=True,
    )
    assert out.returncode == 0, out.stderr
    return tuple(float(v) for v in out.stdout.split())


# Source only the function definition out of the installer by extracting the
# compute_libinput_matrix body with awk, so sourcing does not run the whole
# installer (which parses args + exits).
_SHELL_HARNESS = r"""
set -euo pipefail
fn="$(awk '/^compute_libinput_matrix\(\) \{/{c=1} c{print} c&&/^\}/{exit}' "__SCRIPT__")"
eval "$fn"
compute_libinput_matrix "$1" "$2" "$3" "$4" "$5" "$6" "$7"
"""


@pytest.mark.skipif(shutil.which("bash") is None, reason="bash not available")
@pytest.mark.parametrize(
    "x0,x1,y0,y1,swap,ix,iy",
    [
        (0, 4095, 0, 4095, 0, 0, 0),
        (200, 3900, 200, 3900, 0, 0, 0),
        (150, 3950, 300, 3800, 0, 0, 0),
        (200, 3900, 200, 3900, 1, 0, 0),
        (200, 3900, 200, 3900, 0, 1, 0),
        (200, 3900, 200, 3900, 0, 0, 1),
        (200, 3900, 200, 3900, 1, 1, 1),
    ],
)
def test_shell_matches_python(x0, x1, y0, y1, swap, ix, iy):
    py = matrix_from_bounds(
        x0, x1, y0, y1,
        swap_xy=bool(swap), invert_x=bool(ix), invert_y=bool(iy),
    )
    sh = _shell_matrix(x0, x1, y0, y1, swap, ix, iy)
    assert sh == pytest.approx(py, rel=1e-4, abs=1e-5)
