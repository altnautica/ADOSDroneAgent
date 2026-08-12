"""Air-side video helpers.

The in-process GStreamer pipeline that used to live here is gone: the native
video service owns the encode path on every profile. What remains is the
auto-fallback watcher, which resolves the per-board default for
``video.use_gst_air_pipeline``.

Deliberately imports nothing at package level. ``auto_fallback`` is read from a
pydantic default factory, so it runs on every config load in every Python
service; an eager re-export here would make that path carry whatever else the
package happened to name.
"""

from __future__ import annotations

__all__: list[str] = []
