"""HMAC-SHA256 command signing and verification."""

from __future__ import annotations

import hashlib
import hmac
import os
import struct

from ados.core.logging import get_logger

log = get_logger("hmac-signing")


def generate_secret_key() -> bytes:
    """Generate a 32-byte random secret key for HMAC signing."""
    return os.urandom(32)


class HmacSigner:
    """Signs and verifies payloads using HMAC-SHA256.

    The signature covers both the timestamp and the payload to prevent
    replay attacks and tampering. Used for authenticating commands
    from GCS to the drone agent.
    """

    def __init__(self, secret_key: bytes) -> None:
        if len(secret_key) < 16:
            msg = "HMAC secret key must be at least 16 bytes"
            raise ValueError(msg)
        self._key = secret_key

    def sign(self, payload: bytes, timestamp: float) -> str:
        """Create an HMAC-SHA256 signature over (timestamp || payload).

        Args:
            payload: The raw command bytes to sign.
            timestamp: Unix timestamp (seconds since epoch).

        Returns:
            Hex-encoded HMAC signature string.
        """
        ts_bytes = struct.pack("!d", timestamp)
        message = ts_bytes + payload
        sig = hmac.new(self._key, message, hashlib.sha256).hexdigest()
        return sig

    def verify(self, payload: bytes, timestamp: float, signature: str) -> bool:
        """Verify an HMAC-SHA256 signature.

        Args:
            payload: The raw command bytes that were signed.
            timestamp: The timestamp used during signing.
            signature: The hex-encoded HMAC to verify.

        Returns:
            True if the signature is valid.
        """
        expected = self.sign(payload, timestamp)
        valid = hmac.compare_digest(expected, signature)
        if not valid:
            log.warning("hmac_verification_failed")
        return valid
