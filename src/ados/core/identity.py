"""Device identity management — persistent device ID generation.

On first boot, generates a 12-char hex device ID and persists it to
/etc/ados/device-id. Subsequent boots read the existing ID. Falls back
to ephemeral ID if the filesystem is read-only or the agent runs as
non-root.
"""

from __future__ import annotations

import uuid
from pathlib import Path

from ados.core.logging import get_logger
from ados.core.paths import DEVICE_ID_PATH as _DEVICE_ID_PATH

log = get_logger("core.identity")

DEVICE_ID_PATH = _DEVICE_ID_PATH


def get_or_create_device_id(path: Path | None = None) -> str:
    """Load device ID from disk, or generate and save one on first boot.

    Parameters
    ----------
    path : Path or None
        Override the default persistence path. Useful for testing.

    Returns
    -------
    str
        A 12-character hex device ID (e.g. "a3f7c9e10b42").
    """
    id_path = path or DEVICE_ID_PATH

    if id_path.is_file():
        try:
            existing = id_path.read_text().strip()
            if existing:
                return existing
        except OSError:
            pass

    device_id = uuid.uuid4().hex[:12]
    try:
        id_path.parent.mkdir(parents=True, exist_ok=True)
        id_path.write_text(device_id + "\n")
        log.info("first_boot", device_id=device_id)
    except OSError as e:
        # Running as non-root or read-only filesystem, use ephemeral ID
        log.warning("device_id_not_persisted", error=str(e), device_id=device_id)

    return device_id
