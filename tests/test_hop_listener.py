"""Tests for the GS-side HopListener + HopAnnounce v2 wire format.

The GS listener needs its own history ring + state-file persist so
the LCD ChannelHopsPage has data to render. The drone-side
HopSupervisor is on a separate rig (no LCD); without this surface
the bench LCD would show the empty state forever.
"""

from __future__ import annotations

import json
from pathlib import Path

from ados.services.wfb.hop_supervisor import (
    _HOP_ANNOUNCE_VERSION_CURRENT,
    _TRIGGER_PERIODIC,
    _TRIGGER_REACTIVE,
    HOP_CONTROL_PORT,
    HopAnnounce,
    HopListener,
)

# ---- wire format round-trip ----


def test_announce_v2_encodes_trigger_byte():
    key = b"\x00" * 32
    announce = HopAnnounce(
        version=_HOP_ANNOUNCE_VERSION_CURRENT,
        epoch_ms=1234567890,
        target_channel=149,
        trigger=_TRIGGER_REACTIVE,
    )
    raw = announce.encode(key)
    assert len(raw) == 51
    decoded = HopAnnounce.decode(raw, key)
    assert decoded is not None
    assert decoded.version == 2
    assert decoded.epoch_ms == 1234567890
    assert decoded.target_channel == 149
    assert decoded.trigger == _TRIGGER_REACTIVE
    assert decoded.trigger_label == "reactive"


def test_announce_v1_decodes_as_periodic():
    """An old v1 announce (reserved byte = 0) reads back as periodic."""
    key = b"\x01" * 32
    announce = HopAnnounce(
        version=1,
        epoch_ms=42,
        target_channel=36,
        trigger=0,  # reserved-byte=0 was the v1 default
    )
    raw = announce.encode(key)
    decoded = HopAnnounce.decode(raw, key)
    assert decoded is not None
    assert decoded.version == 1
    assert decoded.trigger == _TRIGGER_PERIODIC
    assert decoded.trigger_label == "periodic"


def test_announce_unknown_trigger_label_falls_back_periodic():
    key = b"\x02" * 32
    announce = HopAnnounce(
        version=2,
        epoch_ms=1,
        target_channel=149,
        trigger=99,  # not in the label map
    )
    raw = announce.encode(key)
    decoded = HopAnnounce.decode(raw, key)
    assert decoded is not None
    assert decoded.trigger == 99
    # Unknown values fall back to the safe "periodic" label rather
    # than throwing — the chart will draw a green marker.
    assert decoded.trigger_label == "periodic"


def test_announce_hmac_mismatch_is_rejected():
    key = b"\x03" * 32
    other_key = b"\x04" * 32
    announce = HopAnnounce(
        version=2, epoch_ms=1, target_channel=149, trigger=0,
    )
    raw = announce.encode(key)
    # Different key on decode -> HMAC fails -> None.
    assert HopAnnounce.decode(raw, other_key) is None


# ---- HopListener history + persist ----


class _StubWfbManager:
    def __init__(self) -> None:
        self._interface = "wlan1"
        self._channel = 149
        self.stopped = False
        self.start_rx_calls: list[int] = []

    async def stop(self) -> None:
        self.stopped = True

    async def start_rx(self, interface: str, channel: int) -> bool:
        self.start_rx_calls.append(channel)
        self._channel = channel
        return True


def test_listener_snapshot_matches_supervisor_shape():
    """snapshot() must surface the same keys as HopSupervisor so the
    LCD page reads the same JSON whichever side wrote the file."""
    listener = HopListener(
        wfb_manager=_StubWfbManager(),
        band="u-nii-3",
        control_port=HOP_CONTROL_PORT,
    )
    snap = listener.snapshot()
    # Keys we know the page + the API expect.
    for key in (
        "enabled",
        "band",
        "history",
        "last_hop_at",
    ):
        assert key in snap
    # GS-side snapshot identifies itself so downstream tooling can
    # distinguish from the drone-side supervisor.
    assert snap["source"] == "listener"


def test_listener_persist_writes_canonical_path(monkeypatch, tmp_path):
    """The persist hook must write the supervisor file path the LCD
    page reads from — same file, same shape."""
    state_path = tmp_path / "hop-supervisor.json"
    monkeypatch.setattr(
        "ados.services.wfb.hop_supervisor.HOP_SUPERVISOR_JSON", state_path
    )
    listener = HopListener(wfb_manager=_StubWfbManager(), band="u-nii-1")
    listener._history.append(
        {
            "at": 12345.0,
            "from": 149,
            "to": 44,
            "trigger": "reactive",
            "ok": True,
        }
    )
    listener._persist_snapshot()
    assert state_path.is_file()
    blob = json.loads(state_path.read_text())
    assert blob["band"] == "u-nii-1"
    assert len(blob["history"]) == 1
    assert blob["history"][0]["trigger"] == "reactive"


def test_listener_persist_swallows_io_errors(monkeypatch):
    """Persist failures must NOT crash the listener loop."""
    # Path that can't possibly be writable.
    bad_path = Path("/proc/cannot-write-here.json")
    monkeypatch.setattr(
        "ados.services.wfb.hop_supervisor.HOP_SUPERVISOR_JSON", bad_path
    )
    listener = HopListener(wfb_manager=_StubWfbManager(), band="u-nii-1")
    # Must not raise.
    listener._persist_snapshot()
