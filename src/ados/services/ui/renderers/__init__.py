"""Render targets for the on-board UI service.

The OLED service renders pure PIL canvases via screen modules in
``ados.services.ui.screens.*``. Each screen takes ``(draw, width,
height, state)`` and paints onto whatever surface the caller provides.

This package adds non-OLED render targets (currently: an SPI LCD via
the kernel ``fbtft`` framebuffer at ``/dev/fb1``). Renderers implement
a tiny Protocol so the service can hold a list and ``present()`` to
each one with the same intermediate image.

Adding a new render target means writing one class with a ``present()``
method and a class-method ``probe()`` that returns ``None`` when the
hardware is absent. The service hooks it in next to the OLED branch
without touching the screen modules.
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Protocol, runtime_checkable

if TYPE_CHECKING:  # pragma: no cover - typing-only import
    from PIL.Image import Image


@runtime_checkable
class Renderer(Protocol):
    """A surface the OLED service can paint onto.

    The service draws into a PIL Image at the screen-friendly logical
    size (128x64) and hands it to ``present()``. Renderers are
    responsible for any scaling, color-space conversion, or hardware
    flush they need to display that image.
    """

    name: str
    width: int
    height: int

    def present(self, image: Image) -> None:
        """Display the image. Called from the render loop at ~5 Hz."""

    def cleanup(self) -> None:
        """Release any hardware resources before the service exits."""


__all__ = ["Renderer"]
