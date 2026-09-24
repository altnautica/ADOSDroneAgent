"""Tests for detached-signature + sha256 verification (ados.services.signing)."""

from __future__ import annotations

import base64

from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
from cryptography.hazmat.primitives.serialization import (
    Encoding,
    NoEncryption,
    PrivateFormat,
    PublicFormat,
)

from ados.services.signing import (
    verify_signature,
)


def _generate_ed25519_keypair() -> tuple[bytes, bytes]:
    """Generate Ed25519 private + public key PEM bytes."""
    private_key = Ed25519PrivateKey.generate()
    private_pem = private_key.private_bytes(
        Encoding.PEM, PrivateFormat.PKCS8, NoEncryption()
    )
    public_pem = private_key.public_key().public_bytes(
        Encoding.PEM, PublicFormat.SubjectPublicKeyInfo
    )
    return private_pem, public_pem


def test_verify_signature_valid():
    private_pem, public_pem = _generate_ed25519_keypair()

    from cryptography.hazmat.primitives.serialization import load_pem_private_key

    private_key = load_pem_private_key(private_pem, password=None)

    data = b"update payload"
    sig = private_key.sign(data)  # type: ignore[union-attr]
    sig_b64 = base64.b64encode(sig).decode()

    assert verify_signature(data, sig_b64, public_pem) is True


def test_verify_signature_invalid():
    _, public_pem = _generate_ed25519_keypair()

    data = b"update payload"
    fake_sig = base64.b64encode(b"x" * 64).decode()

    assert verify_signature(data, fake_sig, public_pem) is False


def test_verify_signature_wrong_data():
    private_pem, public_pem = _generate_ed25519_keypair()

    from cryptography.hazmat.primitives.serialization import load_pem_private_key

    private_key = load_pem_private_key(private_pem, password=None)

    data = b"original data"
    sig = private_key.sign(data)  # type: ignore[union-attr]
    sig_b64 = base64.b64encode(sig).decode()

    assert verify_signature(b"tampered data", sig_b64, public_pem) is False


def test_verify_signature_bad_key():
    assert verify_signature(b"data", "c2ln", b"not a PEM key") is False




