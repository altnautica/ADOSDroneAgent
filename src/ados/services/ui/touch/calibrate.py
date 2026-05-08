"""5-point touchscreen calibration wizard.

The wizard takes the full 480x320 panel — no chrome, no tab bar — and
prompts the operator to tap five known target positions in sequence.
After the fifth tap, the wizard fits an affine matrix to the five
sample/target pairs and persists the result to ``/etc/ados/touch.calib``.

If the RMS residual exceeds the rejection threshold (the operator
mistapped a target by a wide margin) the wizard returns a failure
result so the OLED service can offer a retry.

The wizard state machine is::

    start() -> step 0
    submit_sample(0, x_raw, y_raw) -> step 1
    ...
    submit_sample(4, x_raw, y_raw) -> step 5 (complete)
    complete() -> Affine | None

Targets are positioned to give the least-squares fit good corner
coverage plus a center reference: corners at (40, 40), (440, 40),
(40, 280), (440, 280) and the panel center at (240, 160).
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path

from PIL import Image, ImageDraw

from ados.core.paths import TOUCH_CALIB_PATH
from ados.services.ui.dashboards.components import primitives as p
from ados.services.ui.theme import Palette

from .transform import (
    Affine,
    compute_from_samples,
)
from .transform import (
    save as save_calib,
)
from .transform import (
    save_skip_marker as save_skip_calib,
)

CANVAS_W = 480
CANVAS_H = 320

# RMS rejection threshold in LCD pixels. A clean tap with a stylus
# produces RMS residuals well under 5 px; a loose finger tap is in the
# 5..15 range; an obvious mistap (operator hit the wrong corner)
# pushes the residual past 50.
REJECT_RMS_PX = 50.0

# Target order matches the mockup at mockups/lcd-ui/pages/07-touch-
# calibrate.html. Coordinates are in LCD pixel space.
TARGETS: tuple[tuple[int, int], ...] = (
    (40, 40),
    (440, 40),
    (240, 160),
    (40, 280),
    (440, 280),
)

STEP_COUNT = len(TARGETS)


@dataclass(frozen=True)
class CalibrationResult:
    """Outcome of a completed wizard run."""

    success: bool
    affine: Affine | None
    rms_px: float
    error: str | None


class CalibrationWizard:
    """5-point calibration state machine + renderer."""

    def __init__(
        self,
        *,
        save_path: Path | None = None,
        rotation: int = 0,
    ) -> None:
        self._step: int = -1  # -1 = not started; 0..STEP_COUNT-1 active
        self._samples: list[tuple[int, int]] = []
        self._save_path = save_path or TOUCH_CALIB_PATH
        self._rotation = rotation

    # ── lifecycle ────────────────────────────────────────────────

    def start(self) -> None:
        """Reset and start at step 0."""
        self._step = 0
        self._samples = []

    def skip(self) -> None:
        """Persist a skip marker so the wizard does not re-launch on boot."""
        save_skip_calib(self._save_path)
        self._step = STEP_COUNT  # treat as terminal

    @property
    def step(self) -> int:
        return self._step

    @property
    def total(self) -> int:
        return STEP_COUNT

    @property
    def is_done(self) -> bool:
        """True once all five samples are in. Caller should call complete()."""
        return self._step >= STEP_COUNT

    @property
    def is_active(self) -> bool:
        return 0 <= self._step < STEP_COUNT

    @property
    def current_target(self) -> tuple[int, int] | None:
        if not self.is_active:
            return None
        return TARGETS[self._step]

    # ── input ────────────────────────────────────────────────────

    def submit_sample(self, step: int, x_raw: int, y_raw: int) -> None:
        """Record a raw ADC sample for the given step.

        The step index is checked so an out-of-order submit (the touch
        bridge fired a stale event after the wizard advanced) does
        not corrupt the sample list.
        """
        if step != self._step:
            return
        if not self.is_active:
            return
        self._samples.append((int(x_raw), int(y_raw)))
        self._step += 1

    def complete(self) -> CalibrationResult:
        """Fit the affine and persist on success.

        Returns a :class:`CalibrationResult`. On failure, the wizard
        leaves the file untouched so the next run starts from the
        same uncalibrated state.
        """
        if not self.is_done:
            return CalibrationResult(
                success=False,
                affine=None,
                rms_px=float("inf"),
                error="incomplete",
            )
        if len(self._samples) != STEP_COUNT:
            return CalibrationResult(
                success=False,
                affine=None,
                rms_px=float("inf"),
                error="sample_count_mismatch",
            )
        try:
            affine, rms = compute_from_samples(
                list(self._samples), list(TARGETS),
            )
        except ValueError as exc:
            return CalibrationResult(
                success=False,
                affine=None,
                rms_px=float("inf"),
                error=str(exc),
            )
        if rms > REJECT_RMS_PX:
            return CalibrationResult(
                success=False,
                affine=None,
                rms_px=rms,
                error="rms_above_threshold",
            )
        save_calib(
            affine,
            self._save_path,
            rotation=self._rotation,
            rms=rms,
            lcd_size=(CANVAS_W, CANVAS_H),
        )
        return CalibrationResult(
            success=True,
            affine=affine,
            rms_px=rms,
            error=None,
        )

    def reset_for_retry(self) -> None:
        """Throw away samples and restart at step 0."""
        self.start()

    # ── rendering ────────────────────────────────────────────────

    def render(self, palette: Palette) -> Image.Image:
        """Paint the full-canvas wizard frame.

        Renders all five target rings (done targets in success color,
        the active target in accent color, pending targets dimmed),
        the centered legend with step counter + instruction, and a
        small skip hint anchored at the bottom edge.
        """
        img = Image.new("RGB", (CANVAS_W, CANVAS_H), palette.bg_primary)
        draw = ImageDraw.Draw(img)

        for i, (tx, ty) in enumerate(TARGETS):
            if i < self._step:
                self._draw_target(draw, tx, ty, palette.status_success, ring_alpha=1.0)
            elif i == self._step:
                self._draw_target(draw, tx, ty, palette.accent_primary, ring_alpha=1.0)
                self._draw_pulse(draw, tx, ty, palette.accent_primary)
            else:
                self._draw_target(draw, tx, ty, palette.text_tertiary, ring_alpha=0.5)

        # Legend block: positioned just below the center target so it
        # doesn't visually collide with it.
        title_font = p.font("sans_bold", 18)
        body_font = p.font("sans_regular", 12)
        step_label_font = p.font("sans_bold", 11)

        step_text = f"STEP {min(self._step, STEP_COUNT - 1) + 1} / {STEP_COUNT}"
        title_text = "Tap each target with the stylus"
        body_text = (
            "Hold the stylus steady. We save the calibration "
            "after the fifth target is tapped."
        )

        legend_y = 200
        # Step label (centered).
        step_w, _ = p.text_size(img, step_text, step_label_font)
        draw.text(
            ((CANVAS_W - step_w) // 2, legend_y),
            step_text,
            fill=palette.accent_primary,
            font=step_label_font,
        )
        # Title (centered).
        title_w, _ = p.text_size(img, title_text, title_font)
        draw.text(
            ((CANVAS_W - title_w) // 2, legend_y + 16),
            title_text,
            fill=palette.text_primary,
            font=title_font,
        )
        # Body (centered, wrapped manually because the line is short).
        body_w, _ = p.text_size(img, body_text, body_font)
        draw.text(
            ((CANVAS_W - body_w) // 2, legend_y + 40),
            body_text,
            fill=palette.text_secondary,
            font=body_font,
        )

        # Skip hint anchored bottom-center.
        skip_text = "long-press anywhere to skip"
        skip_font = p.font("sans_regular", 10)
        skip_w, _ = p.text_size(img, skip_text, skip_font)
        draw.text(
            ((CANVAS_W - skip_w) // 2, CANVAS_H - 20),
            skip_text,
            fill=palette.text_tertiary,
            font=skip_font,
        )
        return img

    def render_failure(self, palette: Palette, rms_px: float) -> Image.Image:
        """Paint a failure card with the measured RMS and a retry hint."""
        img = Image.new("RGB", (CANVAS_W, CANVAS_H), palette.bg_primary)
        draw = ImageDraw.Draw(img)
        title_font = p.font("sans_bold", 22)
        body_font = p.font("sans_regular", 14)
        title_text = "Calibration off"
        body_text = (
            f"Residual {rms_px:.1f} px exceeds the {REJECT_RMS_PX:.0f} px "
            "limit. Tap to retry."
        )
        title_w, _ = p.text_size(img, title_text, title_font)
        body_w, _ = p.text_size(img, body_text, body_font)
        draw.text(
            ((CANVAS_W - title_w) // 2, CANVAS_H // 2 - 30),
            title_text,
            fill=palette.status_warning,
            font=title_font,
        )
        draw.text(
            ((CANVAS_W - body_w) // 2, CANVAS_H // 2 + 4),
            body_text,
            fill=palette.text_secondary,
            font=body_font,
        )
        return img

    # ── private helpers ──────────────────────────────────────────

    def _draw_target(
        self,
        draw: ImageDraw.ImageDraw,
        cx: int,
        cy: int,
        color: tuple[int, int, int],
        *,
        ring_alpha: float,
    ) -> None:
        # Outer ring.
        outer_r = 18
        inner_r = 6
        # Approximate alpha by mixing toward bg_primary; PIL default
        # ImageDraw on RGB cannot blend, so we just keep colors solid
        # and rely on the palette's text_tertiary being visibly dim.
        _ = ring_alpha
        draw.ellipse(
            (cx - outer_r, cy - outer_r, cx + outer_r, cy + outer_r),
            outline=color,
            width=2,
        )
        draw.ellipse(
            (cx - inner_r, cy - inner_r, cx + inner_r, cy + inner_r),
            fill=color,
        )
        # Crosshair through the center.
        draw.line((cx - outer_r - 4, cy, cx - outer_r + 2, cy), fill=color, width=1)
        draw.line((cx + outer_r - 2, cy, cx + outer_r + 4, cy), fill=color, width=1)
        draw.line((cx, cy - outer_r - 4, cx, cy - outer_r + 2), fill=color, width=1)
        draw.line((cx, cy + outer_r - 2, cx, cy + outer_r + 4), fill=color, width=1)

    def _draw_pulse(
        self,
        draw: ImageDraw.ImageDraw,
        cx: int,
        cy: int,
        color: tuple[int, int, int],
    ) -> None:
        # Static halo on the active target. Animation would require
        # a framerate clock in the renderer; the framework can drive
        # animation later by re-rendering in the supervising loop.
        for r in (24, 28, 32):
            draw.ellipse(
                (cx - r, cy - r, cx + r, cy + r),
                outline=color,
                width=1,
            )
