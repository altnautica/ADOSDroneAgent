"""Tests for the non-blocking log shipper to the local logging store.

These cover the four load-bearing properties: the encoded frame matches the
native ingest wire contract byte-for-byte; the handler never blocks and drops
the lowest-severity records first under pressure; an absent or broken socket is
handled silently; and redaction matches the native implementation on a fixed
vector set so the two cannot drift.
"""

from __future__ import annotations

import logging
import os
import queue
import socket
import struct
import tempfile
import threading
import time

import msgpack
import pytest

from ados.core import logd_ship
from ados.core.logd_ship import (
    SOURCE,
    WIRE_VERSION,
    LogdQueueHandler,
    LogdShipper,
    _level_token,
    _redact_value,
    encode_log_frame,
    install_logd_handler,
    uninstall_logd_handler,
)

# --- helpers ---------------------------------------------------------------


def _record(
    level: int = logging.INFO,
    name: str = "ados.test",
    msg: str = "hello",
    args=(),
    **fields,
) -> logging.LogRecord:
    r = logging.LogRecord(
        name=name,
        level=level,
        pathname=__file__,
        lineno=1,
        msg=msg,
        args=args,
        exc_info=None,
    )
    # Caller-supplied structured fields ride as record attributes, the same way
    # stdlib ``extra=`` populates them.
    for k, v in fields.items():
        setattr(r, k, v)
    return r


def _decode_body(frame: bytes) -> dict:
    """Split the 4-byte big-endian length prefix and decode the msgpack body."""
    assert len(frame) >= 4
    (length,) = struct.unpack("!I", frame[:4])
    body = frame[4:]
    assert len(body) == length, "length prefix must equal the body length"
    return msgpack.unpackb(body, raw=False)


# --- wire schema -----------------------------------------------------------


def test_encoded_frame_matches_native_log_schema():
    rec = _record(level=logging.WARNING, name="ados.api", msg="hello world", attempt=3)
    rec.created = 1_700_000_000.0  # exact seconds -> 1_700_000_000_000_000 us
    frame = encode_log_frame(rec)
    body = _decode_body(frame)

    # The discriminator tag and the full key set the native decoder expects.
    assert body["t"] == "log"
    assert body["v"] == WIRE_VERSION == 1
    assert body["ts"] == 1_700_000_000_000_000
    assert body["src"] == SOURCE == "python-agent"
    assert body["lvl"] == "warn"  # lowercase level token, not an integer
    assert body["tgt"] == "ados.api"
    assert body["msg"] == "hello world"
    assert body["f"] == {"attempt": 3}
    # No extra keys beyond the contract.
    assert set(body) == {"t", "v", "ts", "src", "lvl", "tgt", "msg", "f"}


def test_timestamp_is_microsecond_epoch_integer():
    rec = _record()
    rec.created = 1_700_000_000.123456
    body = _decode_body(encode_log_frame(rec))
    assert isinstance(body["ts"], int)
    assert body["ts"] == 1_700_000_000_123_456


def test_level_token_mapping_matches_native_ordinals():
    # The native enum: trace=0 debug=1 info=2 warn=3 error=4. The wire carries
    # the lowercase token; these are the producible Python levels.
    assert _level_token(logging.DEBUG) == "debug"
    assert _level_token(logging.INFO) == "info"
    assert _level_token(logging.WARNING) == "warn"
    assert _level_token(logging.ERROR) == "error"
    # CRITICAL collapses to the most severe wire level.
    assert _level_token(logging.CRITICAL) == "error"
    # Custom/NOTSET levels clamp into range.
    assert _level_token(0) == "trace"
    assert _level_token(100) == "error"


def test_empty_fields_and_target_are_omitted():
    # An empty logger name omits ``tgt``; no extra fields omits ``f`` (matching
    # the native ``skip_serializing_if`` so the encoders agree byte-for-byte).
    rec = _record(name="", msg="m")
    body = _decode_body(encode_log_frame(rec))
    assert "tgt" not in body
    assert "f" not in body
    assert set(body) == {"t", "v", "ts", "src", "lvl", "msg"}


def test_message_args_are_rendered_into_msg():
    rec = _record(msg="value is %d and %s", args=(42, "ok"))
    body = _decode_body(encode_log_frame(rec))
    assert body["msg"] == "value is 42 and ok"


def test_frame_decodes_with_native_protocol_crate():
    """The Python frame is decoded by the native ados-protocol decoder.

    Skips when the Rust toolchain is not present; the byte-schema test above
    still pins the contract in pure Python. When cargo is available this proves
    cross-language byte compatibility end to end.
    """
    import shutil
    import subprocess
    from pathlib import Path

    if shutil.which("cargo") is None:
        pytest.skip("cargo not available")
    crates = Path(__file__).resolve().parents[1] / "crates"
    example = crates / "ados-protocol" / "examples" / "_wire_probe.rs"
    if not example.exists():
        pytest.skip("wire probe example not present")

    rec = _record(level=logging.WARNING, name="ados.api", msg="hello world", attempt=3)
    rec.created = 1_700_000_000.0
    setattr(rec, "api_key", "redacted:ABCD...bb2a0cee")
    frame = encode_log_frame(rec)
    body_hex = frame[4:].hex()

    proc = subprocess.run(
        ["cargo", "run", "-q", "-p", "ados-protocol", "--example", "_wire_probe"],
        cwd=crates,
        input=body_hex,
        capture_output=True,
        text=True,
        timeout=600,
    )
    out = proc.stderr + proc.stdout
    assert "PY_DECODE_OK" in out, f"native decode failed: {out}"
    assert "PY_DECODE_ERR" not in out, out


# --- redaction parity ------------------------------------------------------


# The expected outputs are the native crate's parity vectors. Identical output
# from both implementations is the contract; any drift fails here and in the
# native suite.
_REDACT_VECTORS = [
    ("api_key", "ABCDEFGHIJ1234567890", "redacted:ABCD...bb2a0cee"),
    ("pairing_code", "999888", "redacted:9998...685f188e"),
    ("token", "tok_supersecretvalue", "redacted:tok_...160e465f"),
    ("password", "hunter2", "redacted:hunt...f52fbd32"),
    ("secret", "s", "redacted:s...043a7187"),
    ("device_secret", "0xDEADBEEFCAFE", "redacted:0xDE...c19821b8"),
]


@pytest.mark.parametrize("key,value,expected", _REDACT_VECTORS)
def test_redaction_parity_with_native(key, value, expected):
    assert _redact_value(key, value) == expected
    # Idempotent: feeding the redacted output back yields the same string.
    assert _redact_value(key, expected) == expected


def test_redaction_skips_non_secret_empty_and_sentinel():
    assert _redact_value("device_id", "abc123") == "abc123"
    assert _redact_value("api_key", "") == ""
    once = _redact_value("api_key", "ABCDEFGHIJ1234567890")
    assert _redact_value("api_key", once) == once


def test_secret_field_in_frame_is_shipped_redacted():
    # A field that reached the stdlib record by a non-structlog path is still
    # redacted by the shipper before it goes on the wire.
    rec = _record(msg="auth", api_key="ABCDEFGHIJ1234567890", device_id="abc123")
    body = _decode_body(encode_log_frame(rec))
    assert body["f"]["api_key"] == "redacted:ABCD...bb2a0cee"
    assert body["f"]["device_id"] == "abc123"  # non-secret passes through


def test_non_string_secret_value_is_stringified_then_redacted():
    # A non-str value under a secret-bearing key whose ``str()`` reveals a
    # secret must be stringified and then redacted; the cleartext never ships.
    class Tok:
        def __str__(self):
            return "tok_supersecretvalue"

    rec = _record(msg="auth", session_token=Tok())
    body = _decode_body(encode_log_frame(rec))
    assert body["f"]["session_token"] == "redacted:tok_...160e465f"


# --- non-blocking handler --------------------------------------------------


def test_handler_emit_never_blocks_on_full_queue():
    q: queue.Queue[logging.LogRecord] = queue.Queue(maxsize=2)
    handler = LogdQueueHandler(q)
    start = time.monotonic()
    # Emit far more than the queue can hold; each call must return at once.
    for _ in range(1000):
        handler.emit(_record(level=logging.INFO))
    elapsed = time.monotonic() - start
    assert elapsed < 1.0, "emit must be wait-free even when the queue is full"
    assert q.qsize() == 2  # never grows past the bound


def test_handler_drops_low_severity_first_keeps_warn_plus():
    q: queue.Queue[logging.LogRecord] = queue.Queue(maxsize=2)
    handler = LogdQueueHandler(q)
    # Fill with two droppable INFO records.
    handler.emit(_record(level=logging.INFO, msg="info-1"))
    handler.emit(_record(level=logging.INFO, msg="info-2"))
    assert q.qsize() == 2
    # An incoming WARNING must displace a droppable record, not be lost.
    handler.emit(_record(level=logging.WARNING, msg="warn-1"))
    assert q.qsize() == 2
    levels = sorted(q.get_nowait().levelno for _ in range(2))
    assert logging.WARNING in levels, "the WARNING must be retained"


def test_handler_drops_incoming_low_severity_when_queue_full_of_warnings():
    q: queue.Queue[logging.LogRecord] = queue.Queue(maxsize=2)
    handler = LogdQueueHandler(q)
    handler.emit(_record(level=logging.ERROR, msg="err-1"))
    handler.emit(_record(level=logging.ERROR, msg="err-2"))
    assert q.qsize() == 2
    # Incoming INFO finds the queue full of high-severity records: it is shed.
    handler.emit(_record(level=logging.INFO, msg="info-late"))
    assert q.qsize() == 2
    levels = [q.get_nowait().levelno for _ in range(2)]
    assert all(lv == logging.ERROR for lv in levels), "errors must not be displaced"


def test_warning_does_not_displace_a_queued_error():
    q: queue.Queue[logging.LogRecord] = queue.Queue(maxsize=1)
    handler = LogdQueueHandler(q)
    handler.emit(_record(level=logging.ERROR, msg="err"))
    handler.emit(_record(level=logging.WARNING, msg="warn"))
    assert q.qsize() == 1
    assert q.get_nowait().levelno == logging.ERROR


def test_emit_never_raises_even_on_bad_record():
    q: queue.Queue[logging.LogRecord] = queue.Queue(maxsize=1)
    handler = LogdQueueHandler(q)

    class Boom:
        @property
        def levelno(self):
            raise RuntimeError("boom")

    # A pathological record must not propagate out of emit.
    handler.emit(Boom())  # type: ignore[arg-type]


# --- shipper / socket behaviour -------------------------------------------


def test_shipper_absent_socket_does_not_raise_or_block():
    # Point at a path that does not exist: connecting must fail silently and the
    # shipper must keep draining (and dropping) without raising.
    with tempfile.TemporaryDirectory() as d:
        path = os.path.join(d, "nonexistent", "logd.sock")  # parent missing too
        q: queue.Queue[logging.LogRecord] = queue.Queue(maxsize=8)
        shipper = LogdShipper(q, socket_path=path)
        shipper.start()
        try:
            # Feed records steadily; the shipper drains each against the dead
            # socket without raising. A blocking ``put`` (with a bound) is fine
            # here because the shipper is actively draining the far side.
            for _ in range(20):
                q.put(_record(level=logging.INFO), timeout=2.0)
            # Give the thread time to drain its queue against the dead socket.
            deadline = time.monotonic() + 3.0
            while q.qsize() > 0 and time.monotonic() < deadline:
                time.sleep(0.05)
            assert q.qsize() == 0, "shipper must drain even with no socket"
            assert shipper.is_alive(), "shipper must survive a dead socket"
            assert shipper._shipped == 0  # nothing shipped, nothing crashed
        finally:
            shipper.stop()
            shipper.join(timeout=2.0)
            assert not shipper.is_alive()


def test_shipper_delivers_frames_to_a_live_socket():
    with tempfile.TemporaryDirectory() as d:
        path = os.path.join(d, "logd.sock")
        srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        srv.bind(path)
        srv.listen(1)
        received: list[bytes] = []
        ready = threading.Event()

        def _accept():
            conn, _ = srv.accept()
            ready.set()
            buf = b""
            conn.settimeout(3.0)
            try:
                while len(received) < 1:
                    chunk = conn.recv(65536)
                    if not chunk:
                        break
                    buf += chunk
                    # Parse exactly one length-prefixed frame.
                    if len(buf) >= 4:
                        (length,) = struct.unpack("!I", buf[:4])
                        if len(buf) >= 4 + length:
                            received.append(buf[4 : 4 + length])
            except TimeoutError:
                pass
            finally:
                conn.close()

        acceptor = threading.Thread(target=_accept, daemon=True)
        acceptor.start()

        q: queue.Queue[logging.LogRecord] = queue.Queue(maxsize=8)
        shipper = LogdShipper(q, socket_path=path)
        shipper.start()
        try:
            rec = _record(level=logging.ERROR, name="ados.api", msg="link down")
            q.put_nowait(rec)
            acceptor.join(timeout=4.0)
            assert received, "the shipper did not deliver a frame to the socket"
            body = msgpack.unpackb(received[0], raw=False)
            assert body["t"] == "log"
            assert body["lvl"] == "error"
            assert body["msg"] == "link down"
            assert body["src"] == "python-agent"
        finally:
            shipper.stop()
            shipper.join(timeout=2.0)
            srv.close()


def test_shipper_reconnects_after_socket_reset():
    # The shipper backoff is lazy; force it to retry immediately by zeroing the
    # backoff window after the first failure, then bring the socket up.
    with tempfile.TemporaryDirectory() as d:
        path = os.path.join(d, "logd.sock")
        q: queue.Queue[logging.LogRecord] = queue.Queue(maxsize=8)
        shipper = LogdShipper(q, socket_path=path)
        # First connect fails (no listener yet).
        assert shipper._ensure_connected() is False
        assert shipper._connected is False

        # Bring the listener up and clear the backoff window.
        srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        srv.bind(path)
        srv.listen(1)
        shipper._next_connect_at = 0.0
        try:
            assert shipper._ensure_connected() is True
            assert shipper._connected is True
        finally:
            shipper._close()
            srv.close()


def test_shipper_backoff_advances_and_window_blocks_retry():
    with tempfile.TemporaryDirectory() as d:
        path = os.path.join(d, "logd.sock")
        shipper = LogdShipper(q=queue.Queue(), socket_path=path)
        assert shipper._ensure_connected() is False
        # Inside the backoff window, a second call does not even attempt to
        # connect and returns False immediately.
        assert shipper._next_connect_at > time.monotonic()
        assert shipper._ensure_connected() is False


# --- installation wiring ---------------------------------------------------


def test_install_is_idempotent_and_adds_a_handler():
    uninstall_logd_handler()  # clean slate
    try:
        h1 = install_logd_handler(socket_path="/run/ados/does-not-exist.sock")
        h2 = install_logd_handler(socket_path="/run/ados/does-not-exist.sock")
        assert h1 is h2  # second call is a no-op returning the same handler
        assert h1 in logging.getLogger().handlers
        assert isinstance(h1, LogdQueueHandler)
        assert logd_ship._shipper is not None
        assert logd_ship._shipper.is_alive()
        assert logd_ship._shipper.daemon is True
    finally:
        uninstall_logd_handler()
        assert logd_ship._handler is None


def test_configure_logging_installs_the_shipper():
    uninstall_logd_handler()
    from ados.core.logging import configure_logging

    try:
        configure_logging(level="debug")
        assert logd_ship._handler is not None
        assert logd_ship._handler in logging.getLogger().handlers
        # Emitting a record reaches the queue without needing a socket.
        logging.getLogger("ados.test").warning("a warning")
        time.sleep(0.05)  # let the shipper drain (and drop) it
    finally:
        uninstall_logd_handler()


def test_configure_logging_respects_opt_out(monkeypatch):
    uninstall_logd_handler()
    monkeypatch.setenv("ADOS_LOGD_SHIP", "0")
    from ados.core.logging import configure_logging

    try:
        configure_logging(level="info")
        assert logd_ship._handler is None, "opt-out must skip installation"
    finally:
        uninstall_logd_handler()
