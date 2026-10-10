"""Relay side of the mesh re-key on revocation.

After a revoke the receiver re-seals the rotated bundle to each relay still
paired, under the relay's recorded X25519 key and window code, over the
unicast UDP invite channel. The relay must apply a bundle that opens and is
newer (identity files written, batman restarted) and drop anything else.
"""

from __future__ import annotations

import time
from pathlib import Path

import pytest

from ados.services.ground_station import mesh_rekey, relay_rekey
from ados.services.ground_station import pairing_client as pc
from ados.services.ground_station.invite_crypto import InviteBundle, generate_keypair

CODE = "482910"
RELAY_IP = "10.99.0.7"
RELAY_PORT = 40123


class _FakeChannel:
    """Collects what the receiver sends; delivered by hand to the relay."""

    def __init__(self) -> None:
        self.sent: list[tuple[bytes, tuple[str, int]]] = []

    def send(self, blob: bytes, addr: tuple[str, int]) -> None:
        self.sent.append((blob, addr))


def _bundle(mesh_id: str, psk: bytes, issued_at_ms: int) -> InviteBundle:
    return InviteBundle(
        mesh_id=mesh_id,
        mesh_psk=psk,
        drone_channel=149,
        wfb_rx_key=b"\x66" * 32,
        receiver_mdns_host="receiver-7.local",
        receiver_mdns_port=5800,
        issued_at_ms=issued_at_ms,
        expires_at_ms=issued_at_ms + 120_000,
    )


@pytest.fixture
def relay(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    """A joined relay: identity paths in tmp, stored re-key state, and a
    receiver whose restart hook records calls."""
    mesh_dir = tmp_path / "mesh"
    monkeypatch.setattr(pc, "MESH_DIR", mesh_dir)
    monkeypatch.setattr(pc, "MESH_ID_PATH", mesh_dir / "id")
    monkeypatch.setattr(pc, "MESH_PSK_PATH", mesh_dir / "psk.key")
    monkeypatch.setattr(pc, "RECEIVER_INFO_PATH", mesh_dir / "receiver.json")
    monkeypatch.setattr(pc, "_WFB_RX_KEY_PATH", tmp_path / "wfb" / "rx.key")

    priv, pub = generate_keypair()
    joined_at = int(time.time() * 1000) - 60_000
    state = relay_rekey.RelayRekeyState.from_join(
        priv, CODE, RELAY_PORT, _bundle("ados-00000001", b"\x11" * 32, joined_at)
    )
    relay_rekey.save_state(mesh_dir, state)
    restarts: list[bool] = []

    def _restart() -> bool:
        restarts.append(True)
        return True

    receiver = relay_rekey.RelayRekeyReceiver(
        mesh_dir, relay_rekey.load_state(mesh_dir), pc._persist_bundle, _restart
    )
    record = mesh_rekey.PairedRelay(
        device_id="relay-a", relay_pubkey_hex=pub.hex(), ip=RELAY_IP, port=RELAY_PORT, code=CODE
    )
    return mesh_dir, receiver, record, restarts


def test_a_newer_resealed_bundle_is_applied(relay) -> None:
    mesh_dir, receiver, record, restarts = relay
    channel = _FakeChannel()
    fresh = _bundle("ados-0000beef", b"\x22" * 32, int(time.time() * 1000))

    assert mesh_rekey.reseal(fresh, [record], channel) == ["relay-a"]
    blob, addr = channel.sent[0]
    assert addr == (RELAY_IP, RELAY_PORT)

    assert receiver.handle(blob, ("10.99.0.1", 5801)) is True
    assert (mesh_dir / "id").read_text(encoding="utf-8") == "ados-0000beef\n"
    assert (mesh_dir / "psk.key").read_bytes() == b"\x22" * 32
    assert restarts == [True]
    stored = relay_rekey.load_state(mesh_dir)
    assert stored is not None
    assert stored.issued_at_ms == fresh.issued_at_ms

    # The second copy the receiver sends is not newer: no second restart.
    assert receiver.handle(blob, ("10.99.0.1", 5801)) is False
    assert restarts == [True]


def test_a_bundle_that_does_not_open_is_ignored(relay) -> None:
    mesh_dir, receiver, record, restarts = relay
    channel = _FakeChannel()
    fresh = _bundle("ados-0000dead", b"\x33" * 32, int(time.time() * 1000))
    _other_priv, other_pub = generate_keypair()
    stranger = mesh_rekey.PairedRelay(
        device_id="relay-a",
        relay_pubkey_hex=other_pub.hex(),
        ip=RELAY_IP,
        port=RELAY_PORT,
        code=CODE,
    )
    wrong_code = mesh_rekey.PairedRelay(
        device_id="relay-a",
        relay_pubkey_hex=record.relay_pubkey_hex,
        ip=RELAY_IP,
        port=RELAY_PORT,
        code="000000",
    )

    mesh_rekey.reseal(fresh, [stranger, wrong_code], channel)

    for blob, _addr in channel.sent:
        assert receiver.handle(blob, ("10.99.0.66", 5801)) is False
    assert receiver.handle(b"not an invite", ("10.99.0.66", 5801)) is False
    assert not (mesh_dir / "id").exists()
    assert not (mesh_dir / "psk.key").exists()
    assert restarts == []
