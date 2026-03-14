"""Tests for replay attack detection."""

from __future__ import annotations

import time

from ados.security.replay import ReplayDetector


def test_valid_message():
    detector = ReplayDetector(window_seconds=30.0)
    now = time.time()
    assert detector.check(now, "nonce-1") is True


def test_duplicate_nonce_rejected():
    detector = ReplayDetector(window_seconds=30.0)
    now = time.time()
    assert detector.check(now, "nonce-1") is True
    assert detector.check(now, "nonce-1") is False


def test_expired_timestamp_rejected():
    detector = ReplayDetector(window_seconds=5.0)
    old_time = time.time() - 60
    assert detector.check(old_time, "nonce-old") is False


def test_future_timestamp_rejected():
    detector = ReplayDetector(window_seconds=5.0)
    future = time.time() + 60
    assert detector.check(future, "nonce-future") is False


def test_multiple_unique_nonces():
    detector = ReplayDetector(window_seconds=30.0)
    now = time.time()
    for i in range(10):
        nonce_str = f"nonce-{i}"
        assert detector.check(now, nonce_str) is True


def test_nonce_count():
    detector = ReplayDetector()
    assert detector.nonce_count == 0

    now = time.time()
    detector.check(now, "a")
    detector.check(now, "b")
    assert detector.nonce_count == 2


def test_prune_removes_expired():
    detector = ReplayDetector(window_seconds=1.0)

    # Add a nonce with an old timestamp (mocked)
    detector._nonces["old-nonce"] = time.time() - 10
    detector._nonces["recent-nonce"] = time.time()

    removed = detector.prune()
    assert removed == 1
    assert "old-nonce" not in detector._nonces
    assert "recent-nonce" in detector._nonces


def test_auto_prune_on_overflow():
    detector = ReplayDetector(window_seconds=30.0, max_nonces=5)
    now = time.time()

    # Add old entries manually
    for i in range(3):
        detector._nonces[f"old-{i}"] = now - 60

    # Trigger auto-prune by exceeding max_nonces
    for i in range(4):
        detector.check(now, f"new-{i}")

    # Old entries should have been pruned
    assert detector.nonce_count <= 5


def test_window_property():
    detector = ReplayDetector(window_seconds=42.0)
    assert detector.window_seconds == 42.0
