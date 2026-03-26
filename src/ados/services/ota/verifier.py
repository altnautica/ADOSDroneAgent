"""Cryptographic verification for OTA update packages."""

from __future__ import annotations

import base64
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
        log.error(
            "sha256_mismatch",
            path=filepath,
            expected=expected_hash,
            actual=actual,
        )

    return match


def verify_signature(
    data: bytes, signature_b64: str, public_key_pem: bytes
) -> bool:
    """Verify Ed25519 signature over data using a PEM public key."""
    try:
        from cryptography.hazmat.primitives.asymmetric.ed25519 import (
            Ed25519PublicKey,
        )
        from cryptography.hazmat.primitives.serialization import (
            load_pem_public_key,
        )

        pub_key = load_pem_public_key(public_key_pem)
        if not isinstance(pub_key, Ed25519PublicKey):
            log.error("verify_sig_wrong_key_type")
            return False

        sig = base64.b64decode(signature_b64)
        pub_key.verify(sig, data)
        log.info("signature_verified")
        return True
    except Exception as exc:
        log.error("signature_verification_failed", error=str(exc))
        return False


def load_update_public_key(path: str) -> bytes:
    """Load a PEM public key file. Raises FileNotFoundError if missing."""
    p = Path(path)
    if not p.exists():
        raise FileNotFoundError(f"Public key not found: {path}")
    return p.read_bytes()
