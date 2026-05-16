"""Helpers for the Video page's metrics row + tap-status sidecar.

Pure formatting + I/O helpers split out from the page module so the
class-level orchestration in ``page.py`` stays focused on render +
state-machine logic. Nothing here touches GStreamer; it's all
PIL drawing or atomic JSON writes.
"""

from __future__ import annotations

import json
import os
import tempfile
from pathlib import Path as _Path
from typing import Any


def safe_dict(value: Any) -> dict:
    """Return ``value`` when it is a dict, otherwise an empty dict."""
    return value if isinstance(value, dict) else {}


def format_latency(value: Any) -> str:
    if isinstance(value, (int, float)):
        return f"{int(value)} ms"
    return "--"


def format_rssi(value: Any) -> str:
    if isinstance(value, (int, float)):
        return f"{int(value)} dBm"
    return "--"


def format_bitrate(value: Any) -> str:
    if isinstance(value, (int, float)):
        kbps = float(value)
        if kbps >= 1000:
            return f"{kbps / 1000:.1f} Mbps"
        return f"{kbps:.0f} kbps"
    return "--"


def format_drops(value: Any) -> str:
    """Render an FEC drops cell.

    Tuple form ``(lost, total)`` renders as ``lost / total``. Bare
    int falls through to the legacy ``lost`` display so older cached
    values do not crash the renderer.
    """
    if isinstance(value, tuple) and len(value) == 2:
        lost, total = value
        try:
            return f"{int(lost)} / {int(total)}"
        except (TypeError, ValueError):
            return "--"
    if isinstance(value, (int, float)):
        return str(int(value))
    return "--"


def format_radio(channel: Any, mcs: Any) -> str:
    ch = str(int(channel)) if isinstance(channel, (int, float)) else "--"
    mc = f"MCS{int(mcs)}" if isinstance(mcs, (int, float)) else ""
    if mc:
        return f"ch{ch} {mc}"
    return f"ch{ch}"


def format_channel(value: Any) -> str:
    if isinstance(value, (int, float)) and value > 0:
        return f"ch{int(value)}"
    return "--"


def format_mcs(value: Any) -> str:
    if isinstance(value, (int, float)):
        return f"MCS{int(value)}"
    return "--"


def format_tx_power(value: Any) -> str:
    if isinstance(value, (int, float)):
        return f"{int(value)} dBm"
    return "--"


def write_tap_status(path: Any, blob: dict) -> None:
    """Atomic-write a JSON blob to ``path``.

    Helper for ``VideoPage._publish_tap_status``. Kept module-level so
    the import graph stays acyclic (the page should not depend on the
    cloud heartbeat module to write a sidecar file).
    """
    target = _Path(str(path))
    tmp: str | None = None
    try:
        target.parent.mkdir(parents=True, exist_ok=True)
        fd, tmp = tempfile.mkstemp(
            prefix=target.name + ".",
            suffix=".tmp",
            dir=str(target.parent),
        )
        with os.fdopen(fd, "w") as fh:
            json.dump(blob, fh, separators=(",", ":"))
            fh.flush()
            os.fsync(fh.fileno())
        os.replace(tmp, target)
    except OSError:
        if tmp is not None:
            try:
                os.unlink(tmp)
            except OSError:
                pass
