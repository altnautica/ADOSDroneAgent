"""One-shot camera discovery CLI for the video orchestrator seam.

The Rust video orchestrator shells out to ``python -m ados.hal.camera
--json`` once per stream (re)start to learn which camera is primary and
how to drive the encoder. This module is that subprocess's body: it runs
the real :func:`discover_cameras` (which already applies the
``O_RDWR | O_NONBLOCK`` ghost-node liveness filter for recently-unplugged
USB nodes), runs the same auto-assign heuristic the in-process pipeline
uses (CSI first, then USB, then IP, after filtering non-camera hardware),
and prints exactly one JSON object on stdout.

The output schema is the orchestrator's wire contract:

    {
      "cameras": [
        {"name", "type", "device_path", "width", "height",
         "capabilities", "hardware_role"},
        ...
      ],
      "primary": {"device_path", "name"} | null,
      "total_cameras": N
    }

Per-camera fields are :meth:`CameraInfo.to_dict` verbatim, so the camera
list carries the capability list the encoder reads for input-format
selection. Discovery is best-effort: on any failure the process still
prints a well-formed, empty result and exits 0 so the orchestrator's
no-primary path takes over cleanly instead of seeing a parse error.
"""

from __future__ import annotations

import argparse
import json
import sys

from ados.hal.camera import HardwareRole, discover_cameras
from ados.services.video.camera_mgr import CameraManager


def _build_result() -> dict:
    """Discover cameras, auto-assign roles, and shape the JSON payload.

    Runs a throwaway :class:`CameraManager` exactly the way the in-process
    pipeline does (``set_cameras`` then ``auto_assign``) so the primary the
    orchestrator gets matches the primary the legacy Python pipeline would
    have chosen for the same hardware. Only ``hardware_role == camera``
    entries are eligible to be primary (``auto_assign`` filters internal
    codec / ISP / decoder devices itself).
    """
    cameras = discover_cameras()

    mgr = CameraManager()
    mgr.set_cameras(cameras)
    mgr.auto_assign()
    primary = mgr.get_primary()

    camera_dicts = [c.to_dict() for c in cameras]
    # total_cameras counts only real cameras, matching the pipeline's
    # camera-state ready-gate (internal hardware is never a "camera").
    total = sum(1 for c in cameras if c.hardware_role == HardwareRole.CAMERA)

    primary_block = None
    if primary is not None:
        primary_block = {
            "device_path": primary.device_path,
            "name": primary.name,
        }

    return {
        "cameras": camera_dicts,
        "primary": primary_block,
        "total_cameras": total,
    }


def main(argv: list[str] | None = None) -> int:
    """Print one discovery JSON object to stdout and return an exit code.

    Always exits 0 on a successful (even if empty) discovery; only a
    genuinely broken environment (e.g. import failure) surfaces a
    non-zero code, and even then a well-formed empty payload is written
    so the caller's JSON parse never throws.
    """
    parser = argparse.ArgumentParser(
        prog="ados.hal.camera",
        description="One-shot camera discovery for the video orchestrator.",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="Emit the discovery result as a single JSON object on stdout.",
    )
    parser.parse_args(argv)

    # Route discovery's structlog output to stderr so stdout carries only
    # the JSON object. Without this, the default structlog PrintLogger
    # writes log lines to stdout and the caller would have to scan past
    # them. Best-effort: an import / config failure here is non-fatal.
    try:
        from ados.core.logging import configure_logging

        configure_logging()
    except Exception:  # noqa: BLE001
        pass

    try:
        result = _build_result()
    except Exception:  # noqa: BLE001 - never crash the orchestrator's probe
        result = {"cameras": [], "primary": None, "total_cameras": 0}
        json.dump(result, sys.stdout)
        sys.stdout.write("\n")
        return 1

    json.dump(result, sys.stdout)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
