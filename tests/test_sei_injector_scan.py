"""Annex-B start-code search in the SEI injector.

The injector splices a SEI in front of every slice, so the offset and length
of each start code decide where the SEI lands; a wrong length splits the
start code and corrupts the NAL that follows.
"""

from __future__ import annotations

import pytest

from ados.services.video.sei_injector import _find_start_code


@pytest.mark.parametrize(
    ("buf", "start", "expected"),
    [
        (b"\x00\x00\x00\x01\x65", 0, (0, 4)),
        (b"\x00\x00\x01\x65", 0, (0, 3)),
        (b"\xaa\x00\x00\x00\x01\x65", 0, (1, 4)),
        # A zero before ``start`` is not part of the code found from ``start``.
        (b"\x00\x00\x00\x01\x65", 1, (1, 3)),
        (b"\x00\x00\x01\x41\x00\x00\x00\x01\x65", 4, (4, 4)),
        (b"\x00\x00\x00", 0, None),
        (b"", 0, None),
    ],
)
def test_start_code_offset_and_length(buf, start, expected) -> None:
    assert _find_start_code(buf, start) == expected
