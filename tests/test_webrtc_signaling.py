"""Tests for WebRTC signaling relay error tracking.

Focused on the broker-side failure surface: paho's publish() may queue
but never reach the broker (ACL deny, broker disconnect mid-publish).
Without on_publish + non-zero-rc tracking the agent reported "answer
published" while GCS spun on the cascade timeout. These tests verify the
relay records the failure and exposes it via last_error so cloud status
can surface it to the operator.
"""

from __future__ import annotations

from unittest.mock import MagicMock

from ados.services.cloud.webrtc_signaling import WebrtcSignalingRelay


def _relay() -> WebrtcSignalingRelay:
    return WebrtcSignalingRelay(
        device_id="test",
        broker="example",
        port=8883,
    )


def test_publish_with_tracking_success_records_pending_mid():
    relay = _relay()
    relay._mqtt = MagicMock()
    info = MagicMock()
    info.rc = 0
    info.mid = 42
    relay._mqtt.publish.return_value = info

    ok = relay._publish_with_tracking(b"answer", label="answer")
    assert ok is True
    assert 42 in relay._pending_publishes
    assert relay.last_error is None


def test_publish_with_tracking_nonzero_rc_records_error():
    relay = _relay()
    relay._mqtt = MagicMock()
    info = MagicMock()
    info.rc = 4  # MQTT_ERR_NO_CONN
    info.mid = 99
    relay._mqtt.publish.return_value = info

    ok = relay._publish_with_tracking(b"answer", label="answer")
    assert ok is False
    assert 99 not in relay._pending_publishes
    assert relay.last_error is not None
    assert relay.last_error["code"] == "publish_rejected"
    assert relay.last_error["detail"] == 4
    assert relay._metrics["publish_errors"] == 1
    assert relay._metrics["publish_nacked"] == 1


def test_publish_with_tracking_exception_records_error():
    relay = _relay()
    relay._mqtt = MagicMock()
    relay._mqtt.publish.side_effect = RuntimeError("broker unreachable")

    ok = relay._publish_with_tracking(b"x", label="answer")
    assert ok is False
    assert relay.last_error is not None
    assert relay.last_error["code"] == "publish_exception"
    assert "broker unreachable" in str(relay.last_error["detail"])
    assert relay._metrics["publish_errors"] == 1


def test_publish_with_tracking_no_mqtt_returns_false():
    relay = _relay()
    relay._mqtt = None
    assert relay._publish_with_tracking(b"x", label="x") is False


def test_record_error_sets_timestamp():
    relay = _relay()
    relay._record_error("test_code", 123)
    assert relay.last_error is not None
    assert relay.last_error["code"] == "test_code"
    assert relay.last_error["detail"] == 123
    assert "ts" in relay.last_error
    assert relay.last_error["ts"] > 0


def test_publish_error_uses_tracking_path():
    """_publish_error must go through _publish_with_tracking so failures
    record last_error rather than getting silently dropped."""
    relay = _relay()
    relay._mqtt = MagicMock()
    info = MagicMock()
    info.rc = 0
    info.mid = 7
    relay._mqtt.publish.return_value = info

    relay._publish_error("whep_failed", 500)
    assert relay._metrics["error_answers_published"] == 1
    assert relay.last_error is None  # success path: no error recorded


def test_publish_error_records_when_publish_rejected():
    relay = _relay()
    relay._mqtt = MagicMock()
    info = MagicMock()
    info.rc = 4
    info.mid = 0
    relay._mqtt.publish.return_value = info

    relay._publish_error("whep_failed", 500)
    # error_answers_published only increments on successful publish
    assert relay._metrics["error_answers_published"] == 0
    assert relay.last_error is not None
    assert relay.last_error["code"] == "publish_rejected"
