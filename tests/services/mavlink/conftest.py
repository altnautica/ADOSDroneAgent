"""Shared fixtures for the encoder test suite.

Uses pymavlink as the reference decoder so each round-trip test is a
true contract check against the upstream MAVLink generator output.
"""

from __future__ import annotations

import pytest
from pymavlink.dialects.v20 import ardupilotmega as _ap


@pytest.fixture
def decoder():
    """Return a callable that parses a single MAVLink v2 frame.

    ``mav.parse_char(buf)`` accepts a bytes blob, feeds the parser
    state machine, and returns the decoded message when a full frame
    lands. We assert on a non-None, non-BAD_DATA result so a bad CRC,
    bad length, or unknown dialect surfaces as a real test failure.
    """
    mav = _ap.MAVLink(None, srcSystem=1, srcComponent=1)

    def _decode(frame: bytes):
        msg = mav.parse_char(frame)
        if msg is None:
            raise AssertionError("decoder returned None — frame did not parse")
        msg_type = msg.get_type()
        if msg_type == "BAD_DATA":
            raise AssertionError(f"BAD_DATA: {msg}")
        return msg

    return _decode
