"""Tests for the WFB pair-state manager.

Round-trips the apply / status / unpair lifecycle against a temp key
directory so the tests don't touch /etc/ados/wfb/. Patches
`_systemctl` and `_save_config_dict` so neither real systemd nor the
real /etc/ados/config.yaml is touched. The legacy SHA-256 POC is not
covered: the code path is deleted.
"""

from __future__ import annotations

import asyncio
from pathlib import Path
from unittest.mock import patch

import pytest

from ados.services.ground_station import pair_manager as pm_mod
from ados.services.ground_station.pair_manager import (
    PairKeyError,
    PairManager,
)


@pytest.fixture
def isolated_pm(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> PairManager:
    """A PairManager pointed at a tmp key dir, with all side-effects stubbed."""
    monkeypatch.setattr(pm_mod, "_systemctl", lambda action, unit, **_kw: True)

    saved: dict = {}

    def fake_save(data: dict) -> bool:
        saved.clear()
        saved.update(data)
        return True

    def fake_load() -> dict:
        return dict(saved)

    monkeypatch.setattr(pm_mod, "_save_config_dict", fake_save)
    monkeypatch.setattr(pm_mod, "_load_config_dict", fake_load)
    # Send setup-complete sentinel to a sibling tmp file so the helper
    # doesn't poke /var/lib/ados/.
    monkeypatch.setattr(
        pm_mod, "_SETUP_COMPLETE_PATH", tmp_path / "setup-complete"
    )
    monkeypatch.setattr(
        pm_mod, "_AP_PASSPHRASE_PATH", tmp_path / "ap_passphrase"
    )

    pm_mod._reset_for_tests()
    return PairManager(key_dir=str(tmp_path))


def _make_blob(seed: int = 0xAA) -> bytes:
    return bytes([seed]) * 32 + bytes([seed ^ 0xFF]) * 32


def test_apply_keypair_writes_64_bytes_drone(
    isolated_pm: PairManager, tmp_path: Path
) -> None:
    blob = _make_blob(0x55)
    result = asyncio.run(
        isolated_pm.apply_keypair(blob, "drone", peer_device_id="gs-001")
    )
    tx = tmp_path / "tx.key"
    rx = tmp_path / "rx.key"
    assert tx.is_file()
    assert tx.stat().st_size == 64
    assert tx.read_bytes() == blob
    # Drone-profile apply should NOT touch the rx slot.
    assert not rx.is_file()
    assert result["paired"] is True
    assert result["paired_with_device_id"] == "gs-001"
    assert result["role"] == "drone"
    assert isinstance(result["fingerprint"], str)
    assert len(result["fingerprint"]) == 16


def test_apply_keypair_writes_gs_side(
    isolated_pm: PairManager, tmp_path: Path
) -> None:
    blob = _make_blob(0x33)
    result = asyncio.run(
        isolated_pm.apply_keypair(blob, "gs", peer_device_id="drone-007")
    )
    assert (tmp_path / "rx.key").is_file()
    assert not (tmp_path / "tx.key").is_file()
    assert result["role"] == "gs"
    assert result["paired_with_device_id"] == "drone-007"


def test_apply_keypair_rejects_wrong_size(isolated_pm: PairManager) -> None:
    with pytest.raises(PairKeyError, match="32 bytes, expected 64"):
        asyncio.run(isolated_pm.apply_keypair(b"\x00" * 32, "drone"))


def test_apply_keypair_rejects_non_bytes(isolated_pm: PairManager) -> None:
    with pytest.raises(PairKeyError, match="must be bytes"):
        asyncio.run(isolated_pm.apply_keypair("notbytes", "drone"))  # type: ignore[arg-type]


def test_apply_keypair_persists_state(isolated_pm: PairManager) -> None:
    """After apply, status() reflects the persisted pair fields."""
    blob = _make_blob(0x71)
    asyncio.run(isolated_pm.apply_keypair(blob, "drone", peer_device_id="peer-1"))
    status = asyncio.run(isolated_pm.status("drone"))
    assert status["paired"] is True
    assert status["paired_with_device_id"] == "peer-1"
    assert status["role"] == "drone"
    assert isinstance(status["fingerprint"], str)


def test_unpair_wipes_keys_and_clears_state(
    isolated_pm: PairManager, tmp_path: Path
) -> None:
    blob = _make_blob(0x22)
    asyncio.run(isolated_pm.apply_keypair(blob, "gs", peer_device_id="d-9"))
    assert (tmp_path / "rx.key").is_file()

    result = asyncio.run(isolated_pm.unpair("gs"))
    assert result["paired"] is False
    assert not (tmp_path / "rx.key").is_file()

    status = asyncio.run(isolated_pm.status("gs"))
    assert status["paired"] is False
    assert status["paired_with_device_id"] is None


def test_unpair_wipes_both_files(
    isolated_pm: PairManager, tmp_path: Path
) -> None:
    """A drone-profile unpair should also wipe a stale rx.key, and vice-
    versa; we never want crypto material lingering on disk."""
    (tmp_path / "tx.key").write_bytes(_make_blob(0x11))
    (tmp_path / "rx.key").write_bytes(_make_blob(0x22))

    asyncio.run(isolated_pm.unpair("drone"))
    assert not (tmp_path / "tx.key").is_file()
    assert not (tmp_path / "rx.key").is_file()


def test_apply_flips_auto_pair_off(isolated_pm: PairManager) -> None:
    """apply_keypair must persist auto_pair_enabled=false so the
    supervisor's first-boot loop self-disarms."""
    fake_state: dict = {}

    def fake_save(data: dict) -> bool:
        fake_state.clear()
        fake_state.update(data)
        return True

    with patch.object(pm_mod, "_save_config_dict", side_effect=fake_save):
        asyncio.run(
            isolated_pm.apply_keypair(_make_blob(0xCD), "drone", peer_device_id="x")
        )
    wfb = fake_state.get("video", {}).get("wfb", {})
    assert wfb.get("auto_pair_enabled") is False


def test_set_auto_pair_blocked_when_paired(
    isolated_pm: PairManager,
) -> None:
    """Re-arming on a paired rig must be a soft no-op with a flag."""
    asyncio.run(
        isolated_pm.apply_keypair(_make_blob(0x44), "drone", peer_device_id="x")
    )
    result = asyncio.run(isolated_pm.set_auto_pair(True, "drone"))
    assert result["rearm_blocked"] is True
    assert result["auto_pair_enabled"] is False


def test_set_auto_pair_allowed_when_unpaired(
    isolated_pm: PairManager,
) -> None:
    result = asyncio.run(isolated_pm.set_auto_pair(True, "drone"))
    assert result["auto_pair_enabled"] is True


def test_status_when_no_key_present(isolated_pm: PairManager) -> None:
    status = asyncio.run(isolated_pm.status("gs"))
    assert status["paired"] is False
    assert status["fingerprint"] is None
    assert status["paired_with_device_id"] is None


def test_status_rejects_wrong_size_key(
    isolated_pm: PairManager, tmp_path: Path
) -> None:
    """A truncated/oversized key file must NOT count as paired."""
    (tmp_path / "rx.key").write_bytes(b"\x00" * 32)
    status = asyncio.run(isolated_pm.status("gs"))
    assert status["paired"] is False


def test_factory_reset_rearms_auto_pair(
    isolated_pm: PairManager, tmp_path: Path
) -> None:
    asyncio.run(
        isolated_pm.apply_keypair(_make_blob(0x77), "gs", peer_device_id="d")
    )
    result = asyncio.run(isolated_pm.factory_reset("gs"))
    assert result["reset"] is True
    assert result["auto_pair_enabled"] is True
    assert not (tmp_path / "rx.key").is_file()
