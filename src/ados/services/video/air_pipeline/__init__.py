"""Air-side GStreamer pipeline package — re-exports the public surface.

The original ``air_pipeline.py`` module was split into:

* ``pipeline.py`` — :class:`AirPipeline`, the in-process GStreamer
  pipeline + restart watchdogs.
* ``pipeline_builder.py`` — pure functions that compose the pipeline
  launch string and pick the camera source / encoder element.
* ``stats.py`` — :class:`AirPipelineStats` snapshot container.
* ``errors.py`` — :class:`AirPipelineUnavailable`.

Existing callers (``from ados.services.video.air_pipeline import
AirPipeline, AirPipelineUnavailable``) keep working unchanged.
"""

from __future__ import annotations

from .errors import AirPipelineUnavailable
from .pipeline import AirPipeline
from .pipeline_builder import (
    _gst_element_available,  # noqa: F401  re-exported for test patches
    _read_tx_bytes,  # noqa: F401  re-exported for test patches
    _resolve_wfb_iface,  # noqa: F401  re-exported for test patches
    build_air_pipeline_string,
    choose_camera_source,
    choose_encoder,
)
from .stats import AirPipelineStats

__all__ = [
    "AirPipeline",
    "AirPipelineStats",
    "AirPipelineUnavailable",
    "build_air_pipeline_string",
    "choose_camera_source",
    "choose_encoder",
]
