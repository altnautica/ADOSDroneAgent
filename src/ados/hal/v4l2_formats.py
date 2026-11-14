"""What a V4L2 capture node really offers: its formats, sizes and frame rates,
and the kernel driver behind it.

Formats come from ``v4l2-ctl --list-formats-ext -d <node>`` (the
``VIDIOC_ENUM_FMT`` / ``ENUM_FRAMESIZES`` / ``ENUM_FRAMEINTERVALS`` walk) and
are reported as one entry per format and size::

    {"fourcc": "mjpeg", "width": 1280, "height": 720, "fps": [30.0, 15.0]}

``fourcc`` is the lowercase fourcc with trailing spaces dropped, ``MJPG``
reading as ``mjpeg`` (the name the encoders key on). A stepwise or continuous
size range is reported at its largest size and an interval range at its
highest rate. A format the driver lists without sizes is one entry at 0x0.
``fps`` is highest first, empty when the driver reports no intervals.
"""

from __future__ import annotations

import os
import re
import subprocess
from typing import Any

# Bounded so a wedged node cannot eat the discovery budget the video
# orchestrator gives the whole enumeration.
_PROBE_TIMEOUT_S = 3

_FORMAT_RE = re.compile(r"^\s*\[\d+\]:\s*'([^']{1,4})'")
_SIZE_RE = re.compile(
    r"^\s*Size:\s*(?:Discrete|Stepwise|Continuous)\s+(\d+)x(\d+)(?:\s*-\s*(\d+)x(\d+))?"
)
_INTERVAL_RE = re.compile(r"^\s*Interval:.*\(([\d.]+)(?:\s*-\s*([\d.]+))?\s*fps\)")


def fourcc_name(code: str) -> str:
    """The capability name of a V4L2 fourcc (``'MJPG'`` -> ``"mjpeg"``)."""
    name = code.rstrip().lower()
    return "mjpeg" if name == "mjpg" else name


def parse_list_formats_ext(text: str) -> list[dict[str, Any]]:
    """Parse ``v4l2-ctl --list-formats-ext`` output into format entries."""
    entries: list[dict[str, Any]] = []
    fourcc: str | None = None
    sized = False
    current: dict[str, Any] | None = None
    for line in text.splitlines():
        m = _FORMAT_RE.match(line)
        if m:
            if fourcc is not None and not sized:
                entries.append({"fourcc": fourcc, "width": 0, "height": 0, "fps": []})
            fourcc, sized, current = fourcc_name(m.group(1)), False, None
            continue
        m = _SIZE_RE.match(line)
        if m and fourcc is not None:
            w, h = (m.group(3), m.group(4)) if m.group(3) else (m.group(1), m.group(2))
            current = {"fourcc": fourcc, "width": int(w), "height": int(h), "fps": []}
            entries.append(current)
            sized = True
            continue
        m = _INTERVAL_RE.match(line)
        if m and current is not None:
            fps = float(m.group(2) or m.group(1))
            if fps > 0 and fps not in current["fps"]:
                current["fps"].append(fps)
    if fourcc is not None and not sized:
        entries.append({"fourcc": fourcc, "width": 0, "height": 0, "fps": []})
    for entry in entries:
        entry["fps"].sort(reverse=True)
    return entries


def probe_formats(device_path: str) -> list[dict[str, Any]]:
    """The format entries ``device_path`` offers; empty when it cannot be read."""
    try:
        result = subprocess.run(
            ["v4l2-ctl", "--list-formats-ext", "-d", device_path],
            capture_output=True,
            text=True,
            timeout=_PROBE_TIMEOUT_S,
        )
    except (OSError, subprocess.TimeoutExpired):
        return []
    return parse_list_formats_ext(result.stdout or "")


def capabilities(formats: list[dict[str, Any]]) -> list[str]:
    """The distinct fourcc names of ``formats``, in the driver's order."""
    seen: list[str] = []
    for entry in formats:
        if entry["fourcc"] not in seen:
            seen.append(entry["fourcc"])
    return seen


def best_mode(formats: list[dict[str, Any]]) -> tuple[int, int, float]:
    """The largest offered size and its highest rate, ``(0, 0, 0.0)`` if none."""
    best = (0, 0, 0.0)
    for entry in formats:
        fps = entry["fps"][0] if entry["fps"] else 0.0
        mode = (entry["width"], entry["height"], fps)
        if (mode[0] * mode[1], mode[2]) > (best[0] * best[1], best[2]):
            best = mode
    return best


def v4l2_driver(device_path: str, sys_root: str = "/sys") -> str | None:
    """The kernel driver bound behind a ``/dev/videoN`` node (``uvcvideo``,
    ``sunxi-vin``), or None when sysfs does not say."""
    node = os.path.basename(device_path.rstrip("/"))
    link = os.path.join(sys_root, "class", "video4linux", node, "device", "driver")
    try:
        target = os.readlink(link)
    except OSError:
        return None
    return os.path.basename(target.rstrip("/")) or None
