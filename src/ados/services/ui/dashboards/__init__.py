"""Native-resolution dashboards for the SPI LCD render path.

The OLED service used to scale the 128x64 OLED screen modules onto
the 480x320 SPI LCD as a stop-gap. That works but wastes ~80% of the
panel area. The dashboards here render directly at native resolution
following the ADOS dark-first design language: tiled boxes with
status dots, large numeric values for at-a-glance reading, muted
secondary text for detail.

Each dashboard module exposes a ``render(state) -> PIL.Image``
function. The caller (oled_service) hands in the same state dict the
existing screens consume; the dashboard returns a 480x320 RGB image
that the FrameBufferRenderer blits straight to /dev/fb1.
"""

from __future__ import annotations

from .groundnode_landscape import render as render_groundnode_landscape

__all__ = ["render_groundnode_landscape"]
