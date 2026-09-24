"""Tests for the secret-redaction structlog processor."""

from __future__ import annotations

from ados.core.logging import _REDACT_PREFIX, redact_secrets


def _run(event_dict):
    """Invoke the processor with the same signature structlog would use."""
    return redact_secrets(None, "info", dict(event_dict))


def test_secret_field_keeps_only_its_length():
    out = _run({"code": "ABCDEF"})
    assert out["code"] == f"{_REDACT_PREFIX}len=6"
    # Nothing of the content survives: no head, no digest a reader could
    # brute-force a short secret against.
    assert "ABC" not in out["code"]


def test_redaction_is_idempotent():
    once = _run({"code": "ABCDEF"})
    twice = _run(once)
    assert once["code"] == twice["code"]


def test_int_value_untouched():
    out = _run({"status_code": 200})
    assert out["status_code"] == 200


def test_non_secret_key_untouched():
    out = _run({"device_id": "abc123"})
    assert out["device_id"] == "abc123"


def test_empty_string_untouched():
    out = _run({"code": ""})
    # Empty values have no traceability; processor leaves them alone.
    assert out["code"] == ""


def test_same_length_secrets_are_indistinguishable():
    # A short secret must not be recoverable, so two different values of the
    # same length redact identically.
    a = _run({"code": "999888"})
    b = _run({"code": "123456"})
    assert a["code"] == b["code"] == f"{_REDACT_PREFIX}len=6"
