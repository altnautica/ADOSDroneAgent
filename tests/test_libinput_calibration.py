"""The overlay installer's libinput calibration matrix.

``compute_libinput_matrix`` in ``scripts/drivers/install-display-overlay.sh``
writes the touch calibration matrix at install time. These vectors pin its
output for the bounds, swap and inversion cases.
"""

from __future__ import annotations

import shutil
import subprocess
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[1]
OVERLAY_SCRIPT = REPO_ROOT / "scripts" / "drivers" / "install-display-overlay.sh"


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


# Expected matrices for the libinput calibration formula: scale = ADC_MAX /
# (max - min), offset = -min / (max - min), with swap and inversion applied.
_GOLDEN = [
    ((0, 4095, 0, 4095, 0, 0, 0), (1.0, 0.0, 0.0, 0.0, 1.0, 0.0)),
    ((200, 3900, 200, 3900, 0, 0, 0), (1.106757, 0.0, -0.054054, 0.0, 1.106757, -0.054054)),
    ((150, 3950, 300, 3800, 0, 0, 0), (1.077632, 0.0, -0.039474, 0.0, 1.17, -0.085714)),
    ((200, 3900, 200, 3900, 1, 0, 0), (0.0, 1.106757, -0.054054, 1.106757, 0.0, -0.054054)),
    ((200, 3900, 200, 3900, 0, 1, 0), (-1.106757, 0.0, 1.054054, 0.0, 1.106757, -0.054054)),
    ((200, 3900, 200, 3900, 0, 0, 1), (1.106757, 0.0, -0.054054, 0.0, -1.106757, 1.054054)),
    ((200, 3900, 200, 3900, 1, 1, 1), (0.0, -1.106757, 1.054054, -1.106757, 0.0, 1.054054)),
]


@pytest.mark.skipif(shutil.which("bash") is None, reason="bash not available")
@pytest.mark.parametrize("bounds,expected", _GOLDEN)
def test_installer_matrix_matches_the_formula(bounds, expected):
    assert _shell_matrix(*bounds) == pytest.approx(expected, rel=1e-4, abs=1e-5)
