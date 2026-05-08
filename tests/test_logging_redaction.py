"""Tests for the secret-redaction structlog processor."""

from __future__ import annotations

from ados.core.logging import _REDACT_PREFIX, redact_secrets


def _run(event_dict):
    """Invoke the processor with the same signature structlog would use."""
    return redact_secrets(None, "info", dict(event_dict))


def test_secret_field_is_hashed():
    out = _run({"code": "ABCDEF"})
    assert out["code"].startswith(f"{_REDACT_PREFIX}ABCD...")
    # Format: "redacted:ABCD..." + 8 hex chars
    suffix = out["code"][len(f"{_REDACT_PREFIX}ABCD...") :]
    assert len(suffix) == 8
    int(suffix, 16)  # raises if not hex


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


def test_distinct_values_distinguishable():
    a = _run({"code": "ABCDEF"})
    b = _run({"code": "ABCXYZ"})
    assert a["code"] != b["code"]
    # Both share the 4-char head plus the prefix, but the trailing hash differs.
    assert a["code"].startswith(f"{_REDACT_PREFIX}ABCD...")
    assert b["code"].startswith(f"{_REDACT_PREFIX}ABCX...")
