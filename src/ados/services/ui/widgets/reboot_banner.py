"""28 px reboot banner painted at the top of any page that has pending reboot-required changes.

The banner is NOT a modal. It's a drawable strip that page renderers
call before painting their normal content. The settings page tallies a
``pending_reboot_count`` in shared state every time the operator
commits a reboot-required setting (channel change, MCS index, role
flip). The banner pulls that counter out of state and renders an
amber strip with copy + a tappable "Reboot now" affordance on the
right.

Returning the hit zone lets the caller include it in its own
``hit_zones()`` list so a tap dispatches back to a confirm dialog →
reboot REST call.
"""

from __future__ import annotations

from PIL import Image, ImageDraw

from ados.services.ui.dashboards.components import primitives as p
from ados.services.ui.pages.base import HitZone
from ados.services.ui.theme import Palette

BANNER_H = 28


def draw_reboot_banner(
    image: Image.Image,
    x: int,
    y: int,
    w: int,
    *,
    palette: Palette,
    count: int,
    banner_zone_id: str = "banner.reboot",
) -> HitZone:
    """Paint a 28 px banner and return the hit zone for "Reboot now".

    ``count`` drives the body text: 1 → "1 setting requires a reboot",
    n>1 → "n settings require a reboot". The banner uses the warning
    color from the palette so it reads as advisory rather than error.
    """
    d = ImageDraw.Draw(image)
    h = BANNER_H

    d.rectangle(
        (x, y, x + w - 1, y + h - 1),
        fill=palette.status_warning,
    )
    if count <= 1:
        body = "1 setting requires a reboot"
    else:
        body = f"{count} settings require a reboot"
    body_font = p.font("sans_bold", 12)
    bw, bh = p.text_size(image, body, body_font)
    d.text(
        (x + 12, y + (h - bh) // 2 - 1),
        body,
        fill=palette.bg_primary,
        font=body_font,
    )

    # "Reboot now" call-to-action right-aligned.
    cta = "Reboot now ›"
    cta_font = p.font("sans_bold", 12)
    cw, ch = p.text_size(image, cta, cta_font)
    cx = x + w - 12 - cw
    cy = y + (h - ch) // 2 - 1
    d.text((cx, cy), cta, fill=palette.bg_primary, font=cta_font)

    return HitZone(
        id=banner_zone_id,
        x=cx - 8,
        y=y,
        w=w - (cx - 8 - x),
        h=h,
    )
