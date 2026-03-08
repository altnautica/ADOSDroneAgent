"""Cryptographic verification for OTA update bundles."""

from __future__ import annotations

import base64
import hashlib
from pathlib import Path

from cryptography.exceptions import InvalidSignature
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PublicKey
from cryptography.hazmat.primitives.serialization import load_pem_public_key

from ados.core.logging import get_logger

log = get_logger("ota-verifier")

DEFAULT_PUBLIC_KEY_PATH = "/etc/ados/keys/update-signing.pub"
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


def verify_signature(data: bytes, signature_b64: str, public_key_pem: bytes) -> bool:
    """Verify an Ed25519 signature over data.

    Args:
        data: The raw bytes that were signed.
        signature_b64: Base64-encoded Ed25519 signature.
        public_key_pem: PEM-encoded Ed25519 public key.

    Returns:
        True if the signature is valid, False otherwise.
    """
    try:
        key = load_pem_public_key(public_key_pem)
    except (ValueError, TypeError) as exc:
        log.error("invalid_public_key", error=str(exc))
        return False

    if not isinstance(key, Ed25519PublicKey):
        log.error("wrong_key_type", expected="Ed25519", actual=type(key).__name__)
        return False

    try:
        sig_bytes = base64.b64decode(signature_b64)
    except Exception as exc:
        log.error("invalid_signature_encoding", error=str(exc))
        return False

    try:
        key.verify(sig_bytes, data)
        log.info("signature_verified")
        return True
    except InvalidSignature:
        log.error("signature_invalid")
        return False


def load_update_public_key(path: str = DEFAULT_PUBLIC_KEY_PATH) -> bytes:
    """Load the update-signing public key from disk.

    Args:
        path: Filesystem path to the PEM-encoded public key.

    Returns:
        Raw PEM bytes.

    Raises:
        FileNotFoundError: If the key file does not exist.
    """
    key_path = Path(path)
    if not key_path.exists():
        msg = f"Update signing public key not found: {path}"
        raise FileNotFoundError(msg)

    pem = key_path.read_bytes()
    log.info("public_key_loaded", path=path)
    return pem
