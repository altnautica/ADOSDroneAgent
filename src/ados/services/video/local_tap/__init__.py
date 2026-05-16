"""LCD video tap package — re-exports the public surface.

The original ``local_tap.py`` module was split into:

* ``tap.py`` — :class:`LocalVideoTap`, the async-friendly facade and
  associated lifecycle constants.
* ``pipeline_string.py`` — pipeline-launch builder, decoder selection,
  ``gst-inspect-1.0`` cache.
* ``sei_parser.py`` — pure byte-level SEI parser + UUID constant.
* ``frame_slot.py`` — threadsafe single-slot frame holder.

Existing callers (``from ados.services.video.local_tap import
LocalVideoTap``) keep working unchanged. ``_remove_emulation_prevention``
is intentionally re-exported because the SEI test suite imports it
directly from the package path.
"""

from __future__ import annotations

# ``time`` is re-exported because legacy tests reach in via
# ``local_tap.time`` and patch ``monotonic`` on it; keeping the name
# bound at the package level preserves that workflow.
import time  # noqa: F401

from .frame_slot import _FrameSlot
from .pipeline_string import (
    _INSPECTOR,
    _detect_soc,
    _PluginInspector,
    build_pipeline_string,
    gst_plugin_available,
    select_decoder,
)
from .sei_parser import (
    ADOS_LATENCY_SEI_UUID,
    _iter_nal_units,
    _remove_emulation_prevention,
    parse_sei_latency_ns,
)
from .tap import (
    DEFAULT_HEIGHT,
    DEFAULT_RTSP_URL,
    DEFAULT_WIDTH,
    LocalVideoTap,
    LocalVideoTapUnavailable,
    log,
)

__all__ = [
    "ADOS_LATENCY_SEI_UUID",
    "DEFAULT_HEIGHT",
    "DEFAULT_RTSP_URL",
    "DEFAULT_WIDTH",
    "LocalVideoTap",
    "LocalVideoTapUnavailable",
    "_FrameSlot",
    "_INSPECTOR",
    "_PluginInspector",
    "_detect_soc",
    "_iter_nal_units",
    "_remove_emulation_prevention",
    "build_pipeline_string",
    "gst_plugin_available",
    "log",
    "parse_sei_latency_ns",
    "select_decoder",
]
