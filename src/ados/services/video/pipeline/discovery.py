"""Camera discovery sidecar for :class:`VideoPipeline`.

Holds the single camera-enumeration helper that runs whenever the
pipeline cold-starts. Split out so the lazy package resolution (the
``_pkg()`` route used to honour test patches of
``discover_cameras``) is reviewable on its own, and so future work
on hot-plug discovery has a stable seam to grow into without
touching the orchestrator class body.

The mixin holds methods only — every attribute the methods touch is
declared in :class:`VideoPipeline.__init__` over in ``pipeline.py``.
"""

from __future__ import annotations


class _DiscoveryMixin:
    """Camera discovery helpers grafted onto :class:`VideoPipeline`."""

    def _discover_and_assign(self) -> None:
        """Run camera discovery and auto-assign roles."""
        from .pipeline import _pkg

        cameras = _pkg().discover_cameras()
        self._camera_mgr.set_cameras(cameras)
        self._camera_mgr.auto_assign()
