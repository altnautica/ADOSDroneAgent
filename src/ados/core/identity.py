"""Device identity management — persistent device ID generation and resolution.

On first boot, generates a 12-char hex device ID and persists it to
/etc/ados/device-id. Subsequent boots read the existing ID. Falls back
to ephemeral ID if the filesystem is read-only or the agent runs as
non-root.

A node has exactly one identity: the full id held in the device-id file.
``resolve_device_id`` is the single reader every surface goes through, with
the same precedence as the Rust ``ados_protocol::identity`` resolver.
"""

from __future__ import annotations

import os
import uuid
from pathlib import Path

from ados.core.logging import get_logger
from ados.core.paths import DEVICE_ID_PATH as _DEVICE_ID_PATH

log = get_logger("core.identity")

DEVICE_ID_PATH = _DEVICE_ID_PATH

DEVICE_ID_PATH_ENV = "ADOS_DEVICE_ID_PATH"
DEVICE_ID_ENV = "ADOS_DEVICE_ID"


def device_id_path() -> Path:
    """The device-id file: ``ADOS_DEVICE_ID_PATH`` if set and non-empty, else
    the platform default (``/etc/ados/device-id`` on Linux)."""
    override = os.environ.get(DEVICE_ID_PATH_ENV, "")
    return Path(override) if override.strip() else DEVICE_ID_PATH


def _read_id_file(path: Path) -> str:
    try:
        return path.read_text().strip()
    except (OSError, UnicodeDecodeError):
        return ""


def resolve_device_id(configured: str | None = None, path: Path | None = None) -> str:
    """Resolve this node's device id. Never truncates.

    Precedence (identical to the Rust resolver):

    1. The trimmed contents of the device-id file (``path`` if given, else
       ``ADOS_DEVICE_ID_PATH``, else ``/etc/ados/device-id``), if non-empty.
    2. The trimmed ``ADOS_DEVICE_ID`` env var, if non-empty.
    3. The trimmed ``configured`` value (config.yaml ``agent.device_id``), if
       non-empty. Only dev hosts without the file land here.
    4. ``""``.
    """
    from_file = _read_id_file(path or device_id_path())
    if from_file:
        return from_file
    from_env = os.environ.get(DEVICE_ID_ENV, "").strip()
    if from_env:
        return from_env
    return (configured or "").strip()


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
