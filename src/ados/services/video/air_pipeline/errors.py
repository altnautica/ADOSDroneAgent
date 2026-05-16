"""Exception type for the in-process air-side GStreamer pipeline."""

from __future__ import annotations


class AirPipelineUnavailable(RuntimeError):  # noqa: N818
    """Raised when the in-process GStreamer pipeline cannot run.

    Carries a short reason the caller can surface (``python3-gi``
    missing, no compatible encoder, etc.) so :func:`start_stream` can
    fall back to the legacy bash pipeline cleanly.
    """
