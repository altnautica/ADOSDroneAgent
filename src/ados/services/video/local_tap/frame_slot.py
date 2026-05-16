"""Single-slot threadsafe frame holder shared between the GStreamer
appsink callback and the asyncio render loop.
"""

from __future__ import annotations

import threading

from PIL import Image


class _FrameSlot:
    """Single-slot atomic frame holder.

    The appsink callback runs on the gstreamer streaming thread; the
    page renderer runs in the asyncio loop. We don't need a queue —
    only the latest frame matters. A bare ``threading.Lock`` around a
    single attribute is enough; the lock is held only while swapping
    the reference, never during decode.
    """

    def __init__(self) -> None:
        self._frame: Image.Image | None = None
        self._lock = threading.Lock()

    def set(self, frame: Image.Image | None) -> None:
        with self._lock:
            self._frame = frame

    def get(self) -> Image.Image | None:
        with self._lock:
            return self._frame
