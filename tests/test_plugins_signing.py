"""Plugin Ed25519 signing verifier tests."""

from __future__ import annotations

import base64
from pathlib import Path

import pytest
from cryptography.hazmat.primitives.asymmetric.ed25519 import (
    Ed25519PrivateKey,
)
from cryptography.hazmat.primitives.serialization import (
    Encoding,
    PrivateFormat,
    NoEncryption,
    PublicFormat,
)

from ados.plugins.errors import SignatureError
from ados.plugins.signing import (
    is_first_party_signer,
    load_revocation_list,
    load_trusted_keys,
    verify_archive_signature,
)


@pytest.fixture
def keypair():
    sk = Ed25519PrivateKey.generate()
    pk = sk.public_key()
    pem = pk.public_bytes(Encoding.PEM, PublicFormat.SubjectPublicKeyInfo)
    return sk, pem


def _sign(sk: Ed25519PrivateKey, payload: bytes) -> str:
    return base64.b64encode(sk.sign(payload)).decode("ascii")


def test_verify_happy_path(tmp_path: Path, keypair) -> None:
    sk, pem = keypair
    keys_dir = tmp_path / "keys"
    keys_dir.mkdir()
    (keys_dir / "altnautica-test.pem").write_bytes(pem)

    payload = b"deterministic payload hash" * 2
    sig = _sign(sk, payload)
    trusted = load_trusted_keys(keys_dir)
    verify_archive_signature(payload, sig, "altnautica-test", trusted_keys=trusted, revocations=set())


def test_verify_unknown_signer(tmp_path: Path, keypair) -> None:
    sk, pem = keypair
    keys_dir = tmp_path / "keys"
    keys_dir.mkdir()
    (keys_dir / "altnautica-test.pem").write_bytes(pem)

    payload = b"x" * 32
    sig = _sign(sk, payload)
    trusted = load_trusted_keys(keys_dir)
    with pytest.raises(SignatureError) as ei:
        verify_archive_signature(
            payload, sig, "stranger", trusted_keys=trusted, revocations=set()
        )
    assert ei.value.kind == SignatureError.KIND_UNKNOWN_SIGNER


def test_verify_revoked_signer(tmp_path: Path, keypair) -> None:
    _, pem = keypair
    keys_dir = tmp_path / "keys"
    keys_dir.mkdir()
    (keys_dir / "altnautica-test.pem").write_bytes(pem)
    trusted = load_trusted_keys(keys_dir)
    with pytest.raises(SignatureError) as ei:
        verify_archive_signature(
            b"x" * 32,
            "QUJDRA==",
            "altnautica-test",
            trusted_keys=trusted,
            revocations={"altnautica-test"},
        )
    assert ei.value.kind == SignatureError.KIND_REVOKED


def test_verify_invalid_signature(tmp_path: Path, keypair) -> None:
    sk, pem = keypair
    keys_dir = tmp_path / "keys"
    keys_dir.mkdir()
    (keys_dir / "altnautica-test.pem").write_bytes(pem)

    payload = b"intended payload"
    sig = _sign(sk, payload)
    trusted = load_trusted_keys(keys_dir)
    with pytest.raises(SignatureError) as ei:
        verify_archive_signature(
            b"a different payload",
            sig,
            "altnautica-test",
            trusted_keys=trusted,
            revocations=set(),
        )
    assert ei.value.kind == SignatureError.KIND_INVALID


def test_load_revocation_list_missing(tmp_path: Path) -> None:
    assert load_revocation_list(tmp_path / "absent.json") == set()


def test_load_revocation_list_parses(tmp_path: Path) -> None:
    p = tmp_path / "rev.json"
    p.write_text('["altnautica-2025-A", "third-party-bad"]', encoding="utf-8")
    assert load_revocation_list(p) == {"altnautica-2025-A", "third-party-bad"}


def test_first_party_predicate() -> None:
    # Allowlist members
    assert is_first_party_signer("altnautica-2026-A")
    assert is_first_party_signer("altnautica-2026-B")
    # Non-members (would have passed the old prefix-only check)
    assert not is_first_party_signer("third-party-X")
    assert not is_first_party_signer("altnautic-impostor")
    assert not is_first_party_signer("altnautica-malicious")
    assert not is_first_party_signer("altnautica-2027-A")  # not yet allowlisted
