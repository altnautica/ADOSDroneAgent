"""External binary dependency checker for ADOS services."""

from __future__ import annotations

import shutil
from dataclasses import dataclass

from ados.core.logging import get_logger

log = get_logger("core.deps")


@dataclass
class DependencyStatus:
    name: str
    required: bool
    found: bool
    path: str | None


def check_video_dependencies() -> list[DependencyStatus]:
    """Check external binaries needed by the video pipeline."""
    checks = [
        ("mediamtx", True),
        ("ffmpeg", False),
        ("rpicam-vid", False),
        ("v4l2-ctl", False),
        ("gst-launch-1.0", False),
    ]
    results = []
    for name, required in checks:
        path = shutil.which(name)
        results.append(DependencyStatus(
            name=name,
            required=required,
            found=path is not None,
            path=path,
        ))
    return results
