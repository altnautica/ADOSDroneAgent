"""Tests for HMAC-SHA256 command signing."""

from __future__ import annotations

import time

import pytest

from ados.security.hmac_signing import HmacSigner, generate_secret_key


def test_generate_secret_key():
    key = generate_secret_key()
    assert isinstance(key, bytes)
    assert len(key) == 32


def test_generate_unique_keys():
    k1 = generate_secret_key()
    k2 = generate_secret_key()
    assert k1 != k2


def test_sign_returns_hex():
    signer = HmacSigner(generate_secret_key())
    sig = signer.sign(b"test payload", time.time())
    assert isinstance(sig, str)
    assert len(sig) == 64  # SHA-256 hex = 64 chars
    int(sig, 16)  # Must be valid hex


def test_verify_valid():
    key = generate_secret_key()
    signer = HmacSigner(key)
    ts = time.time()
    payload = b"arm command"

    sig = signer.sign(payload, ts)
    assert signer.verify(payload, ts, sig) is True


def test_verify_wrong_payload():
    key = generate_secret_key()
    signer = HmacSigner(key)
    ts = time.time()

    sig = signer.sign(b"original", ts)
    assert signer.verify(b"tampered", ts, sig) is False


def test_verify_wrong_timestamp():
    key = generate_secret_key()
    signer = HmacSigner(key)

    payload = b"takeoff"
    sig = signer.sign(payload, 1000.0)
    assert signer.verify(payload, 1001.0, sig) is False


def test_verify_wrong_key():
    signer1 = HmacSigner(generate_secret_key())
    signer2 = HmacSigner(generate_secret_key())

    ts = time.time()
    payload = b"data"

    sig = signer1.sign(payload, ts)
    assert signer2.verify(payload, ts, sig) is False


def test_verify_wrong_signature():
    signer = HmacSigner(generate_secret_key())
    ts = time.time()
    assert signer.verify(b"data", ts, "0" * 64) is False


def test_short_key_rejected():
    with pytest.raises(ValueError, match="at least 16"):
        HmacSigner(b"short")


def test_deterministic():
    key = generate_secret_key()
    signer = HmacSigner(key)
    ts = 1234567890.0
    payload = b"hello"

    sig1 = signer.sign(payload, ts)
    sig2 = signer.sign(payload, ts)
    assert sig1 == sig2
