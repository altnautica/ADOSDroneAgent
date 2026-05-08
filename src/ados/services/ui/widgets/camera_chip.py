"""Camera switch chip overlay for the video page.

A 32x80 chip that paints in the top-right of the video region. Shows
the active camera label (e.g. ``CAM 1``) plus a small ``·N`` count
badge when the agent has enumerated more than one camera. When only
one camera is present the chip is hidden entirely so the operator
isn't drawn into a no-op picker.
"""

from __future__ import annotations

from PIL import Image, ImageDraw

from ados.services.ui.dashboards.components import primitives as p
from ados.services.ui.pages.base import HitZone
from ados.services.ui.theme import Palette

WIDTH = 80
HEIGHT = 32


def draw_camera_chip(
    image: Image.Image,
    x: int,
    y: int,
    *,
    palette: Palette,
    label: str,
    count: int,
    hidden: bool = False,
) -> HitZone | None:
    """Paint the camera chip; return its hit zone or ``None`` when hidden.

    When ``hidden`` is True or ``count`` <= 1 the chip is not drawn and
    the function returns ``None`` so the caller can skip zone
    registration.
    """
    if hidden or count <= 1:
        return None

    draw = ImageDraw.Draw(image)
    bg = palette.bg_secondary
    outline = palette.text_secondary
    draw.rectangle(
        (x, y, x + WIDTH - 1, y + HEIGHT - 1),
        fill=bg,
        outline=outline,
        width=1,
    )
    label_font = p.font("sans_bold", 11)
    badge_font = p.font("mono_regular", 10)
    badge = f"·{count}"

    label_w, label_h = p.text_size(image, label, label_font)
    badge_w, badge_h = p.text_size(image, badge, badge_font)
    total_w = label_w + 4 + badge_w
    start_x = x + (WIDTH - total_w) // 2
    label_y = y + (HEIGHT - label_h) // 2 - 1
    draw.text(
        (start_x, label_y),
        label,
        fill=palette.text_primary,
        font=label_font,
    )
    badge_x = start_x + label_w + 4
    badge_y = y + (HEIGHT - badge_h) // 2 - 1
    draw.text(
        (badge_x, badge_y),
        badge,
        fill=palette.accent_primary,
        font=badge_font,
    )

    return HitZone(id="video.cam_chip", x=x, y=y, w=WIDTH, h=HEIGHT)
