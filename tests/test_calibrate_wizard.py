"""Tests for the 5-point touch calibration wizard."""

from __future__ import annotations

from pathlib import Path

from PIL import Image

from ados.services.ui.theme import DARK
from ados.services.ui.touch.calibrate import (
    REJECT_RMS_PX,
    STEP_COUNT,
    TARGETS,
    CalibrationWizard,
)
from ados.services.ui.touch.transform import load as load_calib


def test_wizard_starts_at_step_zero(tmp_path: Path):
    w = CalibrationWizard(save_path=tmp_path / "touch.calib")
    w.start()
    assert w.step == 0
    assert w.is_active
    assert not w.is_done
    assert w.current_target == TARGETS[0]


def test_wizard_advances_through_five_steps(tmp_path: Path):
    w = CalibrationWizard(save_path=tmp_path / "touch.calib")
    w.start()
    # Submit five well-spread raw samples that match the targets
    # exactly under the identity-on-raw mapping.
    samples = [
        (100, 100),
        (3995, 100),
        (2047, 2047),
        (100, 3995),
        (3995, 3995),
    ]
    for i, (xr, yr) in enumerate(samples):
        w.submit_sample(i, xr, yr)
    assert w.is_done
    assert w.step == STEP_COUNT


def test_wizard_complete_succeeds_with_clean_samples(tmp_path: Path):
    target = tmp_path / "touch.calib"
    w = CalibrationWizard(save_path=target)
    w.start()
    samples = [
        (100, 100),
        (3995, 100),
        (2047, 2047),
        (100, 3995),
        (3995, 3995),
    ]
    for i, (xr, yr) in enumerate(samples):
        w.submit_sample(i, xr, yr)
    result = w.complete()
    assert result.success is True
    assert result.affine is not None
    assert result.rms_px < 5.0
    # File should now be loadable.
    assert load_calib(target) is not None


def test_wizard_complete_fails_on_high_rms(tmp_path: Path):
    target = tmp_path / "touch.calib"
    w = CalibrationWizard(save_path=target)
    w.start()
    # All samples land at the panel center — the resulting affine
    # is degenerate and the residual blows up.
    for i in range(STEP_COUNT):
        w.submit_sample(i, 2047, 2047)
    result = w.complete()
    assert result.success is False
    assert result.affine is None
    # File should NOT have been written on rejection.
    assert load_calib(target) is None


def test_wizard_skip_writes_marker(tmp_path: Path):
    target = tmp_path / "touch.calib"
    w = CalibrationWizard(save_path=target)
    w.skip()
    # Marker present means no auto-relaunch, but load() returns None
    # so the bridge falls back to identity.
    assert target.exists()
    assert load_calib(target) is None


def test_wizard_submit_sample_out_of_order_is_ignored(tmp_path: Path):
    w = CalibrationWizard(save_path=tmp_path / "touch.calib")
    w.start()
    # Wrong step index — should not advance the state machine.
    w.submit_sample(2, 1000, 1000)
    assert w.step == 0


def test_wizard_renders_active_frame(tmp_path: Path):
    w = CalibrationWizard(save_path=tmp_path / "touch.calib")
    w.start()
    img = w.render(DARK)
    assert isinstance(img, Image.Image)
    assert img.size == (480, 320)


def test_wizard_renders_failure_card(tmp_path: Path):
    w = CalibrationWizard(save_path=tmp_path / "touch.calib")
    w.start()
    img = w.render_failure(DARK, rms_px=72.5)
    assert img.size == (480, 320)


def test_wizard_reset_for_retry_clears_samples(tmp_path: Path):
    w = CalibrationWizard(save_path=tmp_path / "touch.calib")
    w.start()
    w.submit_sample(0, 100, 100)
    w.submit_sample(1, 3995, 100)
    w.reset_for_retry()
    assert w.step == 0
    assert not w.is_done


def test_reject_rms_threshold_is_documented():
    # Sanity: the threshold matters and should be > a clean fit by an
    # order of magnitude. If a refactor lowers it accidentally, this
    # catches it.
    assert REJECT_RMS_PX >= 20.0
    assert REJECT_RMS_PX <= 200.0
