"""Video pipeline package — re-exports the public surface.

The original ``pipeline.py`` module was split into:

* ``pipeline.py`` — :class:`VideoPipeline`, the long-lived orchestrator
  the supervisor instantiates.
* ``constants.py`` — module-level tunables (regexes, watchdog
  thresholds, RTP destination).

The pipeline is intentionally NOT split further: every stage shares
state (encoder process handle, mediamtx manager, restart counter) and
the lifecycle has subtle ordering dependencies that are easier to
audit when they live in one class. Helper-only modules ride alongside.

Patching contract
-----------------

Tests reach in via the package path to swap a handful of names:

* ``ados.services.video.pipeline.discover_cameras``
* ``ados.services.video.pipeline.detect_encoder_for_command`` (encoder)
* ``ados.services.video.pipeline.build_encoder_command``
* ``ados.services.video.pipeline.log``

These names are bound on the package barrel below. The inner
``pipeline.py`` resolves them at call time via ``sys.modules``, so a
test patch on the barrel is honoured by the production code.
"""

from __future__ import annotations

# ``asyncio`` is re-exported because the wfb-tee tests reach in via
# ``pl_mod.asyncio`` and patch ``create_subprocess_exec`` on it.
import asyncio  # noqa: F401

# Patch points: the bindings tests reach in to override. Defined on
# the barrel so production code resolving them through the package
# module sees whatever the test substituted.
from ados.core.logging import get_logger
from ados.hal.camera import discover_cameras  # noqa: F401
from ados.services.video.encoder import (  # noqa: F401
    build_encoder_command,
    detect_encoder_for_camera,
)

log = get_logger("video.pipeline")

# Bring the orchestrator class + state enum onto the public path.
from .constants import _HEALTH_CHECK_INTERVAL  # noqa: F401,E402
from .pipeline import (  # noqa: E402
    PipelineState,
    VideoPipeline,
)

__all__ = [
    "PipelineState",
    "VideoPipeline",
    "build_encoder_command",
    "detect_encoder_for_camera",
    "discover_cameras",
    "log",
]
