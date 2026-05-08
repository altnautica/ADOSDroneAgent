"""Read/write helpers for ``/etc/ados/display.conf`` rotation key.

The rotation field is a numeric value (0 / 90 / 180 / 270) the LCD
overlay installer writes when the operator picks an orientation. The
settings page surfaces a row that toggles it; the helper here
preserves any other lines (framebuffer_path, framebuffer_name_expected,
display_id) the installer wrote so a rewrite from the agent does not
clobber the installer's contract.

Atomic write via tmpfile + rename so a power cut mid-write cannot
leave a half-flushed file.
"""

from __future__ import annotations

import os
import tempfile
from pathlib import Path

from ados.core.paths import DISPLAY_CONF_PATH

ALLOWED_ROTATIONS: tuple[int, ...] = (0, 90, 180, 270)


def _parse(path: Path) -> dict[str, str]:
    out: dict[str, str] = {}
    if not path.exists():
        return out
    try:
        for raw in path.read_text().splitlines():
            line = raw.strip()
            if not line or line.startswith("#") or "=" not in line:
                continue
            k, _, v = line.partition("=")
            out[k.strip()] = v.strip()
    except OSError:
        return {}
    return out


def read_rotation(path: Path | None = None) -> int:
    """Return the rotation in degrees (0/90/180/270). Defaults to 0."""
    target = path or DISPLAY_CONF_PATH
    blob = _parse(target)
    raw = blob.get("rotation")
    if raw is None:
        return 0
    try:
        value = int(raw)
    except ValueError:
        return 0
    if value not in ALLOWED_ROTATIONS:
        return 0
    return value


def write_rotation(value: int, path: Path | None = None) -> None:
    """Persist ``rotation=<value>`` while preserving other keys.

    Raises ``ValueError`` for an out-of-range value. Other I/O errors
    propagate; the caller (a settings-row commit handler) is expected
    to surface them as a structured failure result.
    """
    if int(value) not in ALLOWED_ROTATIONS:
        raise ValueError(
            f"rotation must be one of {ALLOWED_ROTATIONS}, got {value}"
        )
    target = path or DISPLAY_CONF_PATH
    target.parent.mkdir(parents=True, exist_ok=True)
    blob = _parse(target)
    blob["rotation"] = str(int(value))
    # Preserve insertion order: keep keys we already had at the front,
    # append rotation at the end if it wasn't there before. Since we
    # mutated the parsed dict above this is just a serialize step.
    body = "\n".join(f"{k}={v}" for k, v in blob.items()) + "\n"

    fd, tmp_path = tempfile.mkstemp(
        prefix=target.name + ".",
        suffix=".tmp",
        dir=str(target.parent),
    )
    try:
        with os.fdopen(fd, "w") as fh:
            fh.write(body)
            fh.flush()
            os.fsync(fh.fileno())
        os.replace(tmp_path, target)
    except OSError:
        try:
            os.unlink(tmp_path)
        except OSError:
            pass
        raise
