"""Tests for the ground-station mesh pairing manager.

Covers the receiver-side Accept window state machine, ECDH keypair
generation, ChaCha20Poly1305 invite encryption / decryption, revocation
list persistence with 0o600 mode, submit / approve flow, expiry, and
snapshot reads. Real cryptography is used (not mocked) for the invite
crypto path so the round-trip is meaningful.

The 60 s default Accept window is overridden to short durations in each
test so the suite finishes in well under a second.
"""

from __future__ import annotations

import json
import os
import stat
import time
from pathlib import Path

import pytest

from ados.services.ground_station import pairing_manager as pm
from ados.services.ground_station.pairing_manager import (
    InviteBundle,
    PairingManager,
    decrypt_invite,
    encrypt_invite,
    generate_keypair,
    is_revoked,
    load_revocations,
    revoke,
    save_revocations,
    unrevoke,
)


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


@pytest.fixture
def tmp_revocations(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """Redirect the revocations JSON path to a tmp file and clear cache."""
    target = tmp_path / "revocations.json"
    monkeypatch.setattr(pm, "REVOCATIONS_PATH", target)
    # Clear the in-memory cache so each test starts clean.
    monkeypatch.setattr(pm, "_REVOCATIONS_CACHE", None)
    monkeypatch.setattr(pm, "_REVOCATIONS_CACHE_TS_NS", 0)
    return target


def _bundle(now_ms: int | None = None) -> InviteBundle:
    if now_ms is None:
        now_ms = int(time.time() * 1000)
    return InviteBundle(
        mesh_id="mesh-test",
        mesh_psk=b"\x11" * 32,
        drone_channel=149,
        wfb_rx_key=b"\x22" * 32,
        receiver_mdns_host="receiver-1.local",
        receiver_mdns_port=5800,
        issued_at_ms=now_ms,
        expires_at_ms=now_ms + 60_000,
    )


# ---------------------------------------------------------------------------
# Construction
# ---------------------------------------------------------------------------


def test_pairing_manager_initial_state() -> None:
    manager = PairingManager()
    assert manager.window is None
    assert manager._priv is None
    assert manager._transport is None


# ---------------------------------------------------------------------------
# ECDH keypair + invite encrypt/decrypt round-trip
# ---------------------------------------------------------------------------


def test_generate_keypair_returns_priv_and_32_byte_pub() -> None:
    priv, pub_bytes = generate_keypair()
    assert priv is not None
    assert isinstance(pub_bytes, bytes)
    assert len(pub_bytes) == 32


def test_invite_encrypt_decrypt_round_trip() -> None:
    receiver_priv, _receiver_pub = generate_keypair()
    relay_priv, relay_pub = generate_keypair()

    bundle = _bundle()
    blob = encrypt_invite(bundle, receiver_priv, relay_pub)

    # Wire format: 32 (receiver pub) + 12 (nonce) + N (ct||tag).
    assert len(blob) >= 32 + 12 + 16

    decoded = decrypt_invite(blob, relay_priv)
    assert decoded.mesh_id == bundle.mesh_id
    assert decoded.mesh_psk == bundle.mesh_psk
    assert decoded.drone_channel == bundle.drone_channel
    assert decoded.wfb_rx_key == bundle.wfb_rx_key
    assert decoded.receiver_mdns_host == bundle.receiver_mdns_host


def test_invite_decrypt_too_short_raises() -> None:
    relay_priv, _ = generate_keypair()
    with pytest.raises(ValueError, match="too short"):
        decrypt_invite(b"\x00" * 10, relay_priv)


def test_invite_decrypt_expired_raises() -> None:
    receiver_priv, _ = generate_keypair()
    relay_priv, relay_pub = generate_keypair()

    now_ms = int(time.time() * 1000)
    expired = InviteBundle(
        mesh_id="mesh-x",
        mesh_psk=b"\x33" * 32,
        drone_channel=149,
        wfb_rx_key=b"\x44" * 32,
        receiver_mdns_host="r.local",
        receiver_mdns_port=5800,
        issued_at_ms=now_ms - 200_000,
        expires_at_ms=now_ms - 100_000,
    )
    blob = encrypt_invite(expired, receiver_priv, relay_pub)
    with pytest.raises(ValueError, match="expired"):
        decrypt_invite(blob, relay_priv)


def test_invite_decrypt_with_wrong_key_fails() -> None:
    receiver_priv, _ = generate_keypair()
    _relay_priv, relay_pub = generate_keypair()
    # The attacker has a different relay_priv that does not match relay_pub.
    attacker_priv, _ = generate_keypair()

    blob = encrypt_invite(_bundle(), receiver_priv, relay_pub)
    with pytest.raises(Exception):
        decrypt_invite(blob, attacker_priv)


# ---------------------------------------------------------------------------
# Revocations
# ---------------------------------------------------------------------------


def test_revocations_initially_empty(tmp_revocations: Path) -> None:
    assert load_revocations() == set()


def test_revoke_persists_to_disk(tmp_revocations: Path) -> None:
    revoke("device-bad-1")
    assert tmp_revocations.is_file()
    data = json.loads(tmp_revocations.read_text(encoding="utf-8"))
    assert "device-bad-1" in data


def test_revoke_writes_with_0o600_mode(tmp_revocations: Path) -> None:
    revoke("device-bad-2")
    mode = stat.S_IMODE(os.stat(tmp_revocations).st_mode)
    assert mode == 0o600


def test_unrevoke_removes_entry(tmp_revocations: Path) -> None:
    revoke("device-on-list")
    assert is_revoked("device-on-list")
    unrevoke("device-on-list")
    assert not is_revoked("device-on-list")


def test_save_revocations_round_trip(tmp_revocations: Path) -> None:
    save_revocations({"a", "b", "c"})
    loaded = load_revocations()
    assert loaded == {"a", "b", "c"}


def test_revocations_reload_handles_corrupt_file(tmp_revocations: Path) -> None:
    tmp_revocations.parent.mkdir(parents=True, exist_ok=True)
    tmp_revocations.write_text("not-valid-json", encoding="utf-8")
    # Force a fresh read by clearing the cache.
    pm._REVOCATIONS_CACHE = None
    pm._REVOCATIONS_CACHE_TS_NS = 0
    assert load_revocations() == set()


# ---------------------------------------------------------------------------
# Accept window: open / close / expire
# ---------------------------------------------------------------------------


async def test_open_and_close_window(
    tmp_revocations: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    manager = PairingManager()

    async def _fake_bind(self, bind_addr: str | None = None) -> bool:
        self._transport = object()  # truthy non-None sentinel
        return True

    monkeypatch.setattr(PairingManager, "_bind_socket", _fake_bind)

    window = await manager.open_window(duration_s=2)
    assert window is not None
    assert manager.window is not None
    assert manager._priv is not None
    assert await manager.is_window_open() is True

    await manager.close_window()
    assert manager.window is None
    assert manager._priv is None


async def test_open_window_bind_failure_raises_and_clears(
    tmp_revocations: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    manager = PairingManager()

    async def _fail_bind(self, bind_addr: str | None = None) -> bool:
        return False

    monkeypatch.setattr(PairingManager, "_bind_socket", _fail_bind)

    with pytest.raises(RuntimeError, match="bind failed"):
        await manager.open_window(duration_s=2)
    # Window must be torn down on bind failure.
    assert manager.window is None


async def test_open_window_idempotent_returns_existing(
    tmp_revocations: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    manager = PairingManager()

    async def _fake_bind(self, bind_addr: str | None = None) -> bool:
        self._transport = object()
        return True

    monkeypatch.setattr(PairingManager, "_bind_socket", _fake_bind)

    first = await manager.open_window(duration_s=5)
    second = await manager.open_window(duration_s=5)
    assert first is second


# ---------------------------------------------------------------------------
# submit_request
# ---------------------------------------------------------------------------


async def test_submit_request_when_window_closed_returns_false(
    tmp_revocations: Path,
) -> None:
    manager = PairingManager()
    accepted = await manager.submit_request(
        "dev-1", b"\x00" * 32, ("10.0.0.5", 5801)
    )
    assert accepted is False


async def test_submit_request_revoked_device_rejected(
    tmp_revocations: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    revoke("dev-banned")

    manager = PairingManager()

    async def _fake_bind(self, bind_addr: str | None = None) -> bool:
        self._transport = object()
        return True

    monkeypatch.setattr(PairingManager, "_bind_socket", _fake_bind)
    await manager.open_window(duration_s=5)

    accepted = await manager.submit_request(
        "dev-banned", b"\x00" * 32, ("10.0.0.6", 5801)
    )
    assert accepted is False
    assert manager.window is not None
    # No pending entry was added.
    assert all(r.device_id != "dev-banned" for r in manager.window.pending)


async def test_submit_request_happy_path_appends_pending(
    tmp_revocations: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    manager = PairingManager()

    async def _fake_bind(self, bind_addr: str | None = None) -> bool:
        self._transport = object()
        return True

    monkeypatch.setattr(PairingManager, "_bind_socket", _fake_bind)
    await manager.open_window(duration_s=5)

    _, relay_pub = generate_keypair()
    accepted = await manager.submit_request("dev-good", relay_pub, ("10.0.0.7", 5801))
    assert accepted is True
    assert manager.window is not None
    pending_ids = [r.device_id for r in manager.window.pending]
    assert "dev-good" in pending_ids


async def test_submit_request_dedupes_same_device(
    tmp_revocations: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    manager = PairingManager()

    async def _fake_bind(self, bind_addr: str | None = None) -> bool:
        self._transport = object()
        return True

    monkeypatch.setattr(PairingManager, "_bind_socket", _fake_bind)
    await manager.open_window(duration_s=5)

    _, relay_pub = generate_keypair()
    await manager.submit_request("dev-dup", relay_pub, ("10.0.0.8", 5801))
    await manager.submit_request("dev-dup", relay_pub, ("10.0.0.8", 5801))

    assert manager.window is not None
    matching = [r for r in manager.window.pending if r.device_id == "dev-dup"]
    assert len(matching) == 1


# ---------------------------------------------------------------------------
# approve
# ---------------------------------------------------------------------------


async def test_approve_unknown_device_returns_none(
    tmp_revocations: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    manager = PairingManager()

    async def _fake_bind(self, bind_addr: str | None = None) -> bool:
        self._transport = object()
        return True

    monkeypatch.setattr(PairingManager, "_bind_socket", _fake_bind)
    await manager.open_window(duration_s=5)

    result = await manager.approve("never-seen", _bundle())
    assert result is None


async def test_approve_happy_path_returns_blob(
    tmp_revocations: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    manager = PairingManager()

    async def _fake_bind(self, bind_addr: str | None = None) -> bool:
        # Simulate a transport that records sendto calls.
        sent: list[tuple[bytes, tuple[str, int]]] = []

        class _FakeTransport:
            def sendto(self, data: bytes, addr: tuple[str, int]) -> None:
                sent.append((data, addr))

            def close(self) -> None:  # noqa: D401
                pass

        self._transport = _FakeTransport()
        self._sent_log = sent  # type: ignore[attr-defined]
        return True

    monkeypatch.setattr(PairingManager, "_bind_socket", _fake_bind)
    await manager.open_window(duration_s=5)

    relay_priv, relay_pub = generate_keypair()
    await manager.submit_request("dev-good", relay_pub, ("10.0.0.9", 5801))

    blob = await manager.approve("dev-good", _bundle())
    assert blob is not None
    # The blob must round-trip through decrypt_invite using the relay's priv.
    decoded = decrypt_invite(blob, relay_priv)
    assert decoded.mesh_id == "mesh-test"

    # And the transport must have recorded at least one send.
    sent_log = manager._sent_log  # type: ignore[attr-defined]
    assert len(sent_log) >= 1
    assert sent_log[0][1] == ("10.0.0.9", 5801)


async def test_approve_when_window_closed_returns_none(
    tmp_revocations: Path,
) -> None:
    manager = PairingManager()
    result = await manager.approve("any-id", _bundle())
    assert result is None


# ---------------------------------------------------------------------------
# snapshot
# ---------------------------------------------------------------------------


async def test_snapshot_when_no_window(tmp_revocations: Path) -> None:
    manager = PairingManager()
    snap = await manager.snapshot()
    assert snap == {"open": False}


async def test_snapshot_with_open_window_and_pending(
    tmp_revocations: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    manager = PairingManager()

    async def _fake_bind(self, bind_addr: str | None = None) -> bool:
        self._transport = object()
        return True

    monkeypatch.setattr(PairingManager, "_bind_socket", _fake_bind)
    await manager.open_window(duration_s=5)

    _, relay_pub = generate_keypair()
    await manager.submit_request("dev-snap", relay_pub, ("192.168.1.5", 5801))

    snap = await manager.snapshot()
    assert snap["open"] is True
    assert snap["pending"][0]["device_id"] == "dev-snap"
    assert snap["pending"][0]["remote_ip"] == "192.168.1.5"
    assert "approvals" in snap
