"""Record button overlay for the video page.

A 32x80 pill-shaped button that paints in the top-left of the video
region. Two visual states:

* **idle** — outlined chip, "REC" label in :attr:`Palette.text_secondary`.
* **recording** — filled :attr:`Palette.status_error` pill, white "REC"
  label with a pulsing white dot to the left whose alpha follows
  ``pulse_phase`` (0..1, derived by the caller from
  ``time.monotonic() % 1.0``).

The widget returns the matching :class:`HitZone` so the page can
register the same rectangle for tap dispatch.
"""

from __future__ import annotations

from PIL import Image, ImageDraw

from ados.services.ui.dashboards.components import primitives as p
from ados.services.ui.pages.base import HitZone
from ados.services.ui.theme import Palette

WIDTH = 80
HEIGHT = 32


def draw_rec_button(
    image: Image.Image,
    x: int,
    y: int,
    *,
    palette: Palette,
    recording: bool,
    pulse_phase: float = 0.0,
) -> HitZone:
    """Paint the REC chip and return its hit zone.

    ``pulse_phase`` is expected in the half-open range ``[0, 1)``.
    Values outside the range are wrapped so a caller passing the raw
    monotonic timestamp still produces a sensible animation.
    """
    draw = ImageDraw.Draw(image)
    phase = pulse_phase - int(pulse_phase)
    if phase < 0:
        phase += 1.0

    label = "REC"
    label_font = p.font("sans_bold", 12)

    if recording:
        bg = palette.status_error
        text_color = palette.text_primary
        draw.rectangle(
            (x, y, x + WIDTH - 1, y + HEIGHT - 1),
            fill=bg,
            outline=text_color,
            width=1,
        )
        # Pulsing dot on the left. A pulse_phase of 0.0 is fully on;
        # 0.5 halves the intensity. We paint a single filled circle
        # whose RGB scales with the phase to fake alpha without an
        # RGBA composite step.
        dot_radius = 5
        dot_cx = x + 16
        dot_cy = y + HEIGHT // 2
        intensity = 0.4 + 0.6 * (1.0 - phase)  # 0.4..1.0
        dot_color = _scale_color(text_color, intensity)
        draw.ellipse(
            (
                dot_cx - dot_radius,
                dot_cy - dot_radius,
                dot_cx + dot_radius,
                dot_cy + dot_radius,
            ),
            fill=dot_color,
        )
        text_w, text_h = p.text_size(image, label, label_font)
        tx = x + 32 + (WIDTH - 32 - text_w) // 2
        ty = y + (HEIGHT - text_h) // 2 - 1
        draw.text((tx, ty), label, fill=text_color, font=label_font)
    else:
        bg = palette.bg_secondary
        outline = palette.text_secondary
        draw.rectangle(
            (x, y, x + WIDTH - 1, y + HEIGHT - 1),
            fill=bg,
            outline=outline,
            width=1,
        )
        text_w, text_h = p.text_size(image, label, label_font)
        tx = x + (WIDTH - text_w) // 2
        ty = y + (HEIGHT - text_h) // 2 - 1
        draw.text((tx, ty), label, fill=outline, font=label_font)

    return HitZone(id="video.rec_button", x=x, y=y, w=WIDTH, h=HEIGHT)


def _scale_color(
    color: tuple[int, int, int], intensity: float,
) -> tuple[int, int, int]:
    """Scale an RGB tuple by ``intensity`` (clamped to [0, 1])."""
    factor = max(0.0, min(1.0, intensity))
    return (
        int(color[0] * factor),
        int(color[1] * factor),
        int(color[2] * factor),
    )
