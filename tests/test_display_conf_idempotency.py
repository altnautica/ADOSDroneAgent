"""Tests for the operator-mutable rotation field in display.conf.

The install-display-overlay.sh installer rewrites /etc/ados/display.conf
on every run. Earlier revisions wrote DEFAULT_ROTATION unconditionally
which silently clobbered an operator who had set rotation via
``ados.services.ui.display_conf.write_rotation()`` or via the LCD
Settings page. The installer now sources scripts/lib/display-conf-
helpers.sh and calls display_conf_preserve_rotation() to keep the
operator's choice across --upgrade.

These tests exercise the helper directly via subprocess so the
installer and the tests verify the same code path.
"""

from __future__ import annotations

import subprocess
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[1]
HELPER_PATH = REPO_ROOT / "scripts" / "lib" / "display-conf-helpers.sh"


@pytest.fixture(autouse=True)
def helper_exists() -> None:
    assert HELPER_PATH.is_file(), (
        f"display-conf-helpers.sh missing at {HELPER_PATH}; the installer cannot source it."
    )


def _call_preserve(conf_path: Path | str, default_rotation: int | str) -> tuple[str, str, int]:
    """Invoke display_conf_preserve_rotation via bash subprocess.

    Returns (stdout, stderr, returncode).
    """
    script = (
        f'. "{HELPER_PATH}"\n'
        f'display_conf_preserve_rotation "{conf_path}" "{default_rotation}"\n'
    )
    proc = subprocess.run(
        ["bash", "-c", script],
        capture_output=True,
        text=True,
        check=False,
    )
    return proc.stdout.strip(), proc.stderr.strip(), proc.returncode


def _write_conf(path: Path, rotation: str | None) -> None:
    lines = [
        "display_id=waveshare35a",
        "board=rock-5c-lite",
        "controller=ILI9486",
        "touch_chip=ADS7846",
        "has_touch=true",
        "resolution=480x320",
        "framebuffer_path=/dev/fb0",
        "framebuffer_name_expected=fb_ili9486",
    ]
    if rotation is not None:
        lines.append(f"rotation={rotation}")
    path.write_text("\n".join(lines) + "\n")


def test_preserves_valid_rotation(tmp_path: Path) -> None:
    conf = tmp_path / "display.conf"
    _write_conf(conf, "0")
    out, err, rc = _call_preserve(conf, 90)
    assert rc == 0
    assert out == "0", f"expected 0, got {out!r} (stderr: {err!r})"


@pytest.mark.parametrize("rotation", ["0", "90", "180", "270"])
def test_all_canonical_rotations_preserved(tmp_path: Path, rotation: str) -> None:
    conf = tmp_path / "display.conf"
    _write_conf(conf, rotation)
    out, _, rc = _call_preserve(conf, 90)
    assert rc == 0
    assert out == rotation


def test_missing_rotation_falls_back_to_default(tmp_path: Path) -> None:
    conf = tmp_path / "display.conf"
    _write_conf(conf, rotation=None)
    out, _, rc = _call_preserve(conf, 90)
    assert rc == 0
    assert out == "90"


def test_missing_file_falls_back_to_default(tmp_path: Path) -> None:
    conf = tmp_path / "does-not-exist.conf"
    out, _, rc = _call_preserve(conf, 270)
    assert rc == 0
    assert out == "270"


def test_malformed_rotation_falls_back_with_warn(tmp_path: Path) -> None:
    conf = tmp_path / "display.conf"
    _write_conf(conf, "abc")
    out, err, rc = _call_preserve(conf, 90)
    assert rc == 0
    assert out == "90"
    assert "ignoring unrecognised rotation" in err.lower()


def test_out_of_range_rotation_falls_back(tmp_path: Path) -> None:
    """45 is a multiple but not a canonical 4-way rotation."""
    conf = tmp_path / "display.conf"
    _write_conf(conf, "45")
    out, err, rc = _call_preserve(conf, 0)
    assert rc == 0
    assert out == "0"
    assert "45" in err  # warning quotes the rejected value


def test_rotation_with_whitespace_is_tolerated(tmp_path: Path) -> None:
    """The awk extract strips whitespace; verify."""
    conf = tmp_path / "display.conf"
    conf.write_text("rotation= 180 \n")
    out, _, rc = _call_preserve(conf, 0)
    assert rc == 0
    assert out == "180"


def test_first_rotation_line_wins(tmp_path: Path) -> None:
    """Defensive: if a malformed file has two rotation lines, the
    first one matches (awk's `exit` after first hit)."""
    conf = tmp_path / "display.conf"
    conf.write_text("rotation=270\nrotation=90\n")
    out, _, rc = _call_preserve(conf, 0)
    assert rc == 0
    assert out == "270"


def test_helper_is_sourceable_idempotently() -> None:
    """Sourcing the helper twice should not error or change behaviour."""
    script = (
        f'. "{HELPER_PATH}"\n'
        f'. "{HELPER_PATH}"\n'
        'display_conf_preserve_rotation "/nonexistent" 90\n'
    )
    proc = subprocess.run(
        ["bash", "-c", script],
        capture_output=True,
        text=True,
        check=False,
    )
    assert proc.returncode == 0
    assert proc.stdout.strip() == "90"
