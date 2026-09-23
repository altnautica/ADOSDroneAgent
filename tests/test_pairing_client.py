"""Tests for the relay-side pairing client.

The relay-side join flow publishes a single `join_completed` pair event when an
invite is decrypted and persisted. Like the receiver-side accept-window events,
that publish must also be mirrored to the cross-process pair-event journal the
native mesh WebSocket tails, so the relay node's completion event reaches the
GCS Hardware tab.

The join also has to authenticate the receiver: a node that heard the join
request can seal an invite to the relay's key, so only an invite sealed under
the receiver's window code, from the receiver's address, may be persisted.

These tests drive `request_join` with the socket / config / persist bits
stubbed and real invite crypto.
"""

from __future__ import annotations

import asyncio
import json
from pathlib import Path

import pytest

from ados.services.ground_station import pair_journal
from ados.services.ground_station import pairing_client as pc
from ados.services.ground_station.invite_crypto import (
    InviteBundle,
    encrypt_invite,
    generate_keypair,
)

CODE = "482910"
RECEIVER_IP = "192.168.1.50"


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


def _bundle(mesh_id: str = "mesh-relay") -> InviteBundle:
    import time

    now_ms = int(time.time() * 1000)
    return InviteBundle(
        mesh_id=mesh_id,
        mesh_psk=b"\x55" * 32,
        drone_channel=149,
        wfb_rx_key=b"\x66" * 32,
        receiver_mdns_host="receiver-7.local",
        receiver_mdns_port=5800,
        issued_at_ms=now_ms,
        expires_at_ms=now_ms + 60_000,
    )


def _stub_join(monkeypatch: pytest.MonkeyPatch, replies: list[tuple[bytes, str]]) -> list:
    """Pin the relay keypair, stub config/send/persist, and deliver `replies`
    (blob, source ip) in order on the join socket. Returns the persisted list."""
    relay_priv, relay_pub = generate_keypair()
    monkeypatch.setattr(pc, "generate_keypair", lambda: (relay_priv, relay_pub))
    monkeypatch.setattr(pc, "load_config", lambda: _StubConfig())
    persisted: list = []
    monkeypatch.setattr(pc, "_persist_bundle", persisted.append)

    async def _no_send(sock, device_id, pubkey, receiver_addr):
        return None

    monkeypatch.setattr(pc, "_send_join_request", _no_send)
    pending = iter(replies)
    loop = asyncio.get_running_loop()

    async def _fake_recvfrom(sock, n):
        try:
            blob, ip = next(pending)
        except StopIteration:
            await asyncio.sleep(3600)
            raise AssertionError("no reply queued") from None
        return blob, (ip, pc.PAIR_UDP_PORT)

    monkeypatch.setattr(loop, "sock_recvfrom", _fake_recvfrom)
    return persisted


def _sealed(relay_pub: bytes, code: str, mesh_id: str) -> bytes:
    receiver_priv, _ = generate_keypair()
    return encrypt_invite(_bundle(mesh_id), receiver_priv, relay_pub, code)


async def test_join_completed_mirrored_to_journal(
    tmp_pair_journal: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A successful relay join journals a `join_completed` pair event with the
    shared envelope, so the native mesh WebSocket sees the relay's completion."""
    replies: list[tuple[bytes, str]] = []
    _stub_join(monkeypatch, replies)
    relay_pub = pc.generate_keypair()[1]
    replies.append((_sealed(relay_pub, CODE, "mesh-relay"), "127.0.0.1"))

    result = await pc.request_join(CODE, receiver_host="127.0.0.1", timeout_s=2.0)

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


async def test_an_invite_sealed_without_the_window_code_is_not_persisted(
    tmp_pair_journal: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A node that heard the join request answers first with its own mesh,
    from the receiver's address. It cannot know the window code, so its invite
    does not open, and the receiver's invite that follows is the one kept."""
    replies: list[tuple[bytes, str]] = []
    persisted = _stub_join(monkeypatch, replies)
    relay_pub = pc.generate_keypair()[1]
    replies.append((_sealed(relay_pub, "000000", "mesh-hostile"), RECEIVER_IP))
    replies.append((_sealed(relay_pub, CODE, "mesh-real"), RECEIVER_IP))

    result = await pc.request_join(CODE, receiver_host=RECEIVER_IP, timeout_s=2.0)

    assert result.ok is True
    assert result.mesh_id == "mesh-real"
    assert [b.mesh_id for b in persisted] == ["mesh-real"]


async def test_a_reply_from_another_host_is_ignored(
    tmp_pair_journal: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A unicast join takes its invite only from the receiver it asked."""
    replies: list[tuple[bytes, str]] = []
    persisted = _stub_join(monkeypatch, replies)
    relay_pub = pc.generate_keypair()[1]
    replies.append((_sealed(relay_pub, CODE, "mesh-hostile"), "192.168.1.66"))
    replies.append((_sealed(relay_pub, CODE, "mesh-real"), RECEIVER_IP))

    result = await pc.request_join(CODE, receiver_host=RECEIVER_IP, timeout_s=2.0)

    assert result.ok is True
    assert [b.mesh_id for b in persisted] == ["mesh-real"]


async def test_the_join_gives_up_after_repeated_unreadable_invites(
    tmp_pair_journal: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Each unreadable invite is an online guess at the code, so the join
    stops after a few instead of letting a hostile node keep guessing."""
    replies: list[tuple[bytes, str]] = []
    persisted = _stub_join(monkeypatch, replies)
    relay_pub = pc.generate_keypair()[1]
    for guess in range(pc.MAX_UNREADABLE_INVITES):
        replies.append((_sealed(relay_pub, f"{guess:06d}", "mesh-hostile"), RECEIVER_IP))
    replies.append((_sealed(relay_pub, CODE, "mesh-too-late"), RECEIVER_IP))

    result = await pc.request_join(CODE, receiver_host=RECEIVER_IP, timeout_s=2.0)

    assert result.ok is False
    assert result.error_code == "E_INVITE_REJECTED"
    assert persisted == []


async def test_a_malformed_code_is_refused_before_anything_is_sent(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    result = await pc.request_join("12345", receiver_host=RECEIVER_IP, timeout_s=0.1)
    assert result.ok is False
    assert result.error_code == "E_BAD_CODE"
