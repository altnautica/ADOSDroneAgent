"""Tests for OTA cryptographic verification."""

from __future__ import annotations

import base64
import hashlib

import pytest
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
from cryptography.hazmat.primitives.serialization import (
    Encoding,
    NoEncryption,
    PrivateFormat,
    PublicFormat,
)

from ados.services.ota.verifier import (
    load_update_public_key,
    verify_sha256,
    verify_signature,
)


def test_verify_sha256_correct(tmp_path):
    content = b"ADOS Drone Agent update bundle"
    filepath = tmp_path / "update.bin"
    filepath.write_bytes(content)

    expected = hashlib.sha256(content).hexdigest()
    assert verify_sha256(str(filepath), expected) is True


def test_verify_sha256_wrong_hash(tmp_path):
    filepath = tmp_path / "update.bin"
    filepath.write_bytes(b"some data")

    assert verify_sha256(str(filepath), "0" * 64) is False


def test_verify_sha256_missing_file():
    assert verify_sha256("/nonexistent/file.bin", "a" * 64) is False


def test_verify_sha256_case_insensitive(tmp_path):
    """SHA-256 comparison lowercases expected hash, so uppercase input matches."""
    content = b"test"
    filepath = tmp_path / "test.bin"
    filepath.write_bytes(content)

    h = hashlib.sha256(content).hexdigest()
    assert verify_sha256(str(filepath), h.upper()) is True  # lowercased before comparison


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


def test_load_update_public_key_success(tmp_path):
    key_file = tmp_path / "update-signing.pub"
    key_file.write_bytes(b"-----BEGIN PUBLIC KEY-----\nfake\n-----END PUBLIC KEY-----\n")

    pem = load_update_public_key(str(key_file))
    assert b"PUBLIC KEY" in pem


def test_load_update_public_key_missing():
    with pytest.raises(FileNotFoundError):
        load_update_public_key("/nonexistent/key.pub")
