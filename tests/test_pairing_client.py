"""Tests for the relay-side pairing client's event-journal seam.

The relay-side join flow publishes a single `join_completed` pair event when an
invite is decrypted and persisted. Like the receiver-side accept-window events,
that publish must also be mirrored to the cross-process pair-event journal the
native mesh WebSocket tails, so the relay node's completion event reaches the
GCS Hardware tab. These tests drive `request_join` with the socket / config /
crypto bits stubbed and assert the `join_completed` event lands in the journal
with the shared `{bus,kind,timestamp_ms,payload}` envelope.
"""

from __future__ import annotations

import asyncio
import json
from pathlib import Path

import pytest

from ados.services.ground_station import pair_journal
from ados.services.ground_station import pairing_client as pc
from ados.services.ground_station.pairing_manager import (
    InviteBundle,
    encrypt_invite,
    generate_keypair,
)


@pytest.fixture
def tmp_pair_journal(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """Redirect the cross-process pair-event journal to a tmp file. The journal
    helper resolves the path inside `pair_journal`, so that is where to patch."""
    target = tmp_path / "pair-events.jsonl"
    monkeypatch.setattr(pair_journal, "PAIR_EVENTS_JSONL", target)
    return target


def _read_journal(path: Path) -> list[dict]:
    if not path.is_file():
        return []
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


class _StubConfig:
    class agent:  # noqa: N801 - mirrors the real config attribute path
        device_id = "relay-test"

    class ground_station:  # noqa: N801
        class mesh:  # noqa: N801
            bat_iface = "bat0"


def _bundle() -> InviteBundle:
    import time

    now_ms = int(time.time() * 1000)
    return InviteBundle(
        mesh_id="mesh-relay",
        mesh_psk=b"\x55" * 32,
        drone_channel=149,
        wfb_rx_key=b"\x66" * 32,
        receiver_mdns_host="receiver-7.local",
        receiver_mdns_port=5800,
        issued_at_ms=now_ms,
        expires_at_ms=now_ms + 60_000,
    )


async def test_join_completed_mirrored_to_journal(
    tmp_pair_journal: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A successful relay join journals a `join_completed` pair event with the
    shared envelope, so the native mesh WebSocket sees the relay's completion."""
    # A fixed relay keypair so the encrypted invite below targets a key the
    # patched generate_keypair will hand back inside request_join.
    relay_priv, relay_pub = generate_keypair()
    monkeypatch.setattr(pc, "generate_keypair", lambda: (relay_priv, relay_pub))

    # Encrypt a real invite to the relay's public key; request_join decrypts it
    # with the relay private key the line above pins.
    receiver_priv, _receiver_pub = generate_keypair()
    blob = encrypt_invite(_bundle(), receiver_priv, relay_pub)

    # Stub the heavy / side-effecting bits: config, the on-disk persist, and the
    # outbound send. Only the recv path needs to deliver the invite blob.
    monkeypatch.setattr(pc, "load_config", lambda: _StubConfig())
    monkeypatch.setattr(pc, "_persist_bundle", lambda bundle: None)

    async def _no_send(sock, device_id, pubkey, receiver_addr):
        return None

    monkeypatch.setattr(pc, "_send_join_request", _no_send)

    # Deliver the invite on the first recv by patching the running loop's
    # datagram recv. The real socket is still created + bound (0.0.0.0) but never
    # actually used for I/O because send + recv are both intercepted.
    loop = asyncio.get_running_loop()

    async def _fake_recvfrom(sock, n):
        return blob, ("127.0.0.1", pc.PAIR_UDP_PORT)

    monkeypatch.setattr(loop, "sock_recvfrom", _fake_recvfrom)

    result = await pc.request_join(receiver_host="127.0.0.1", timeout_s=2.0)

    assert result.ok is True
    assert result.mesh_id == "mesh-relay"

    events = _read_journal(tmp_pair_journal)
    completed = [e for e in events if e["kind"] == "join_completed"]
    assert completed, "the relay join_completed event must be journalled"
    e = completed[0]
    assert e["bus"] == "pair"
    assert isinstance(e["timestamp_ms"], int)
    assert e["payload"] == {
        "mesh_id": "mesh-relay",
        "receiver_host": "receiver-7.local",
    }
