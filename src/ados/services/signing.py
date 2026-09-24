"""Ed25519 detached-signature verification.

Self-contained (base64 + ``cryptography``). The plugin signing layer
(:mod:`ados.plugins.signing`) verifies archive signatures and signed grants
with it.
"""

from __future__ import annotations

import base64

from ados.core.logging import get_logger

log = get_logger("signing-verifier")

HASH_CHUNK_SIZE = 65536


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
