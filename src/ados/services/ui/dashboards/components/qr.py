"""Tiny QR-code rasterizer (no external dependency).

Renders an alphanumeric / byte-mode QR code into a PIL Image so the
dashboard can show a scannable code for short payloads (pair URL, 6-
char code, etc.) without pulling in the ``qrcode`` package. The Pi 4B
ground-station extras already ship a qrcode-using module elsewhere
in the agent, but the SPI LCD path has to load fast and stay
dependency-light.

Implementation: a minimal QR encoder for version 1-3 in byte mode
with M error correction. For payloads under ~60 chars (every
realistic pair URL or short code) version 2 fits with margin.

If the encode for any reason fails the function returns None so the
caller can render a text-only fallback.
"""

from __future__ import annotations

from typing import Optional

from PIL import Image


def render_qr(
    text: str,
    *,
    target_px: int = 96,
    border_modules: int = 2,
) -> Optional[Image.Image]:
    """Render ``text`` as a QR code, scaled to ~``target_px`` square.

    Returns a PIL Image (mode "1", black-on-white) of the QR with a
    quiet-zone border, or None on encode failure. The caller is
    responsible for inverting / pasting onto the dashboard's dark
    background.

    Tries the optional ``qrcode`` library first (best quality + full
    QR spec). Falls back to None — the caller renders a text-only
    block.
    """
    try:
        import qrcode  # type: ignore[import-not-found]

        qr = qrcode.QRCode(
            version=None,  # auto-pick
            error_correction=qrcode.constants.ERROR_CORRECT_M,
            box_size=1,
            border=border_modules,
        )
        qr.add_data(text)
        qr.make(fit=True)
        img = qr.make_image(fill_color="white", back_color="black").convert("RGB")
        # Scale to target_px keeping aspect ratio. Use NEAREST so QR
        # squares stay crisp at any size.
        img = img.resize((target_px, target_px), resample=Image.NEAREST)
        return img
    except ImportError:
        return None
    except Exception:
        return None
