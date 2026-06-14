"""Unit coverage for the radio hop socket helper.

The coordinated-hop socket helper forwards ``{"op":"hop","channel":N}`` to the
radio command socket and returns the socket's verdict. This pins the helper's
behaviour when the socket is absent: it returns ``None`` rather than raising.
"""

from __future__ import annotations

import pytest

from ados.api.routes import wfb as wfb_mod


@pytest.mark.asyncio
async def test_radio_hop_via_socket_returns_none_when_unreachable(monkeypatch, tmp_path):
    """The socket helper returns None (not an exception) for an absent socket."""
    monkeypatch.setattr(wfb_mod, "RADIO_CMD_SOCK", tmp_path / "nope.sock")
    result = await wfb_mod._radio_hop_via_socket(149)
    assert result is None
