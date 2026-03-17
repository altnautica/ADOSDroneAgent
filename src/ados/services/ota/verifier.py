"""SHA-256 verification for OTA update packages."""

from __future__ import annotations

import hashlib
from pathlib import Path

from ados.core.logging import get_logger

log = get_logger("ota-verifier")

HASH_CHUNK_SIZE = 65536


def verify_sha256(filepath: str, expected_hash: str) -> bool:
    """Compute streaming SHA-256 of a file and compare to expected hex digest."""
    h = hashlib.sha256()
    path = Path(filepath)

    if not path.exists():
        log.error("verify_sha256_file_missing", path=filepath)
        return False

    with open(path, "rb") as f:
        while True:
            chunk = f.read(HASH_CHUNK_SIZE)
            if not chunk:
                break
            h.update(chunk)

    actual = h.hexdigest()
    match = actual == expected_hash.lower()

    if match:
        log.info("sha256_verified", path=filepath)
    else:
        log.error("sha256_mismatch", path=filepath, expected=expected_hash, actual=actual)

    return match
