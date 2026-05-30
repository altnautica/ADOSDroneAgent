"""Cloud status and command transport must authenticate with a header.

The agent's key is a secret; it travels in the ``X-ADOS-Key`` request
header, never as a URL query parameter. The status heartbeat and the
command poll/ack moved to the native ``ados-cloud`` crate; the in-process
API path stays in ``core/main.py``. All three must keep the key out of
the URL.
"""

from __future__ import annotations

from pathlib import Path

# Native command poll + ack, and the status heartbeat POST.
_RUST_COMMANDS = Path("crates/ados-cloud/src/bin/ados-cloud.rs")
_RUST_STATUS = Path("crates/ados-cloud/src/loops/heartbeat.rs")
# Surviving in-process API path.
_PY_MAIN = Path("src/ados/core/main.py")


def test_native_command_transport_uses_header_auth() -> None:
    source = _RUST_COMMANDS.read_text()
    assert '"X-ADOS-Key"' in source
    # Only the non-secret device id is a query parameter.
    assert '("deviceId"' in source
    # The key is never a query tuple on the wire.
    assert '("apiKey"' not in source


def test_native_status_heartbeat_uses_header_auth() -> None:
    source = _RUST_STATUS.read_text()
    assert '"X-ADOS-Key"' in source
    assert '("apiKey"' not in source


def test_python_main_keeps_key_out_of_urls() -> None:
    source = _PY_MAIN.read_text()
    assert '"X-ADOS-Key"' in source
    assert 'params={"deviceId": device_id, "apiKey": api_key}' not in source
