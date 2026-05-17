"""Tests for the 9-point touch calibration grid + tightened RMS threshold.

Covers:

* Synthetic 9-point recovery — apply a known affine to the LCD targets
  to produce synthetic raw ADC samples, fit, assert the recovered
  matrix is within tolerance of the input.
* Small-noise tolerance — perturb the synthetic samples with ~5 px
  equivalent jitter, fit, assert RMS lands below the tightened
  threshold (35 px).
* Large-noise rejection — perturb with ~60 px equivalent jitter, fit,
  assert RMS exceeds the tightened threshold so the wizard rejects.
* Lock-step guarantee — the calibrate.py and session.py TARGETS
  tuples agree on a 9-point geometry.
"""

from __future__ import annotations

import random

import pytest

from ados.services.ui.touch import calibrate as calibrate_mod
from ados.services.ui.touch import session as session_mod
from ados.services.ui.touch.calibrate import REJECT_RMS_PX, TARGETS
from ados.services.ui.touch.transform import Affine, compute_from_samples


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _invert_affine(a: Affine) -> Affine:
    """Closed-form inverse of a 2x3 affine.

    Given ``x_lcd = a*x_raw + b*y_raw + c`` and
    ``y_lcd = d*x_raw + e*y_raw + f``, the inverse maps LCD back to
    raw. Used by the synthetic generator to produce raw ADC samples
    from the LCD targets without needing a least-squares fit.
    """
    det = a.a * a.e - a.b * a.d
    if abs(det) < 1e-12:
        raise ValueError("affine is singular, cannot invert")
    inv_a = a.e / det
    inv_b = -a.b / det
    inv_d = -a.d / det
    inv_e = a.a / det
    inv_c = -(inv_a * a.c + inv_b * a.f)
    inv_f = -(inv_d * a.c + inv_e * a.f)
    return Affine(a=inv_a, b=inv_b, c=inv_c, d=inv_d, e=inv_e, f=inv_f)


def _generate_samples(
    target_affine: Affine,
    targets: list[tuple[int, int]],
    *,
    jitter_raw: float = 0.0,
    rng: random.Random | None = None,
) -> list[tuple[int, int]]:
    """Generate raw ADC samples that, when transformed by
    ``target_affine``, land on ``targets``.

    Optionally adds uniform jitter (in raw ADC units, applied
    independently per axis) so the test can simulate noisy stylus
    taps.
    """
    inv = _invert_affine(target_affine)
    out: list[tuple[int, int]] = []
    rng = rng or random.Random(0)
    for x_lcd, y_lcd in targets:
        x_raw_f = inv.a * x_lcd + inv.b * y_lcd + inv.c
        y_raw_f = inv.d * x_lcd + inv.e * y_lcd + inv.f
        if jitter_raw > 0:
            x_raw_f += rng.uniform(-jitter_raw, jitter_raw)
            y_raw_f += rng.uniform(-jitter_raw, jitter_raw)
        out.append((int(round(x_raw_f)), int(round(y_raw_f))))
    return out


# Realistic affine matching a 480x320 panel sitting on a 0..4095 ADC.
# A small cross term + non-zero offset mimics a slightly skewed
# touch overlay, which is what we expect from a real panel.
_KNOWN_AFFINE = Affine(
    a=0.1180,
    b=-0.0030,
    c=2.5,
    d=0.0020,
    e=0.0790,
    f=-1.0,
)

# 5 px of LCD jitter in raw ADC units, using the dominant axis slope
# from _KNOWN_AFFINE (a ~= 0.118, so 1 raw count ~= 0.118 px; 5 px ~=
# 42 raw counts). Use 50 raw counts to stay above the per-axis scale
# and keep the noise generous on both axes.
_SMALL_JITTER_RAW = 50.0

# 60 px of LCD jitter in raw ADC units (~500 raw counts on the x axis).
# Use 600 to clearly clear the 35 px rejection threshold while still
# being a plausible mistap pattern, not garbage.
_LARGE_JITTER_RAW = 600.0


# ---------------------------------------------------------------------------
# Lock-step invariants
# ---------------------------------------------------------------------------


class TestGridInvariants:
    def test_calibrate_targets_is_nine_points(self):
        assert len(calibrate_mod.TARGETS) == 9
        assert calibrate_mod.STEP_COUNT == 9

    def test_session_targets_is_nine_points(self):
        assert len(session_mod.TARGETS) == 9
        assert session_mod.STEP_COUNT == 9

    def test_targets_agree_lockstep(self):
        # Geometry MUST match exactly between the two modules; if they
        # ever drift the REST surface and the LCD wizard will fit to
        # different point sets and produce silently-bad calibrations.
        assert calibrate_mod.TARGETS == session_mod.TARGETS

    def test_rms_threshold_tightened(self):
        # The 9-point fit reduces the noise floor, so the threshold
        # was tightened in lockstep. Both modules must agree.
        assert calibrate_mod.REJECT_RMS_PX == 35.0
        assert session_mod.REJECT_RMS_PX == 35.0


# ---------------------------------------------------------------------------
# Synthetic 9-point fit
# ---------------------------------------------------------------------------


class TestNinePointRecovery:
    def test_perfect_fit_recovers_known_affine(self):
        """With zero noise the fit should recover the synthetic affine
        to floating-point precision and RMS should be ~0."""
        targets = list(TARGETS)
        samples = _generate_samples(_KNOWN_AFFINE, targets, jitter_raw=0.0)
        affine, rms = compute_from_samples(samples, targets)
        assert rms < 1.0, f"perfect-fit RMS should be ~0, got {rms}"
        # Slope coefficients (a, b, d, e) round-trip to ~1e-4; the
        # offset coefficients (c, f) absorb the integer rounding done
        # by _generate_samples (raw samples cast to int) and can drift
        # by ~0.05 since the offset error scales with the typical raw
        # magnitude (~2000 counts) divided by 4095. 0.05 is still well
        # under one LCD pixel of effect.
        for got, want, name in (
            (affine.a, _KNOWN_AFFINE.a, "a"),
            (affine.b, _KNOWN_AFFINE.b, "b"),
            (affine.d, _KNOWN_AFFINE.d, "d"),
            (affine.e, _KNOWN_AFFINE.e, "e"),
        ):
            assert abs(got - want) < 0.001, (
                f"slope coef {name} drifted: got {got}, want {want}"
            )
        for got, want, name in (
            (affine.c, _KNOWN_AFFINE.c, "c"),
            (affine.f, _KNOWN_AFFINE.f, "f"),
        ):
            assert abs(got - want) < 0.05, (
                f"offset coef {name} drifted: got {got}, want {want}"
            )

    def test_small_noise_passes_threshold(self):
        """Jitter ~5 px equivalent should fit comfortably below the
        tightened 35 px RMS threshold."""
        targets = list(TARGETS)
        rng = random.Random(42)
        samples = _generate_samples(
            _KNOWN_AFFINE,
            targets,
            jitter_raw=_SMALL_JITTER_RAW,
            rng=rng,
        )
        _, rms = compute_from_samples(samples, targets)
        assert rms < REJECT_RMS_PX, (
            f"small-noise RMS {rms} should be below threshold "
            f"{REJECT_RMS_PX}"
        )
        # And not absurdly close to the threshold — the small-noise
        # case should comfortably pass, leaving margin for real-world
        # variance.
        assert rms < REJECT_RMS_PX * 0.5, (
            f"small-noise RMS {rms} should leave margin under "
            f"threshold {REJECT_RMS_PX}"
        )

    def test_large_noise_is_rejected(self):
        """Jitter ~60 px equivalent should drive RMS over the
        rejection threshold so the wizard reports a bad fit."""
        targets = list(TARGETS)
        rng = random.Random(1337)
        samples = _generate_samples(
            _KNOWN_AFFINE,
            targets,
            jitter_raw=_LARGE_JITTER_RAW,
            rng=rng,
        )
        _, rms = compute_from_samples(samples, targets)
        assert rms >= REJECT_RMS_PX, (
            f"large-noise RMS {rms} should exceed threshold "
            f"{REJECT_RMS_PX} so the wizard rejects"
        )


# ---------------------------------------------------------------------------
# Wizard integration — make sure the state machine accepts 9 steps end-to-end
# ---------------------------------------------------------------------------


class TestWizardEndToEnd:
    def test_wizard_accepts_nine_samples_and_persists(self, tmp_path):
        """Drive the wizard through all 9 steps with clean synthetic
        samples and confirm it produces a successful CalibrationResult
        with the affine written to disk."""
        targets = list(TARGETS)
        samples = _generate_samples(_KNOWN_AFFINE, targets, jitter_raw=0.0)

        save_path = tmp_path / "touch.calib"
        wizard = calibrate_mod.CalibrationWizard(save_path=save_path)
        wizard.start()
        for i, (x_raw, y_raw) in enumerate(samples):
            assert wizard.step == i
            wizard.submit_sample(i, x_raw, y_raw)
        assert wizard.is_done
        result = wizard.complete()
        assert result.success, f"wizard rejected clean fit: {result.error}"
        assert result.rms_px < 1.0
        assert save_path.exists()

    def test_wizard_rejects_large_noise(self, tmp_path):
        """The wizard should report failure (and leave the file
        untouched) when the operator scatters their taps."""
        targets = list(TARGETS)
        rng = random.Random(9999)
        samples = _generate_samples(
            _KNOWN_AFFINE,
            targets,
            jitter_raw=_LARGE_JITTER_RAW,
            rng=rng,
        )

        save_path = tmp_path / "touch.calib"
        wizard = calibrate_mod.CalibrationWizard(save_path=save_path)
        wizard.start()
        for i, (x_raw, y_raw) in enumerate(samples):
            wizard.submit_sample(i, x_raw, y_raw)
        result = wizard.complete()
        assert not result.success
        assert result.error == "rms_above_threshold"
        assert result.rms_px >= REJECT_RMS_PX
        assert not save_path.exists(), (
            "wizard must not persist a calibration that failed RMS check"
        )


if __name__ == "__main__":
    pytest.main([__file__, "-v"])
