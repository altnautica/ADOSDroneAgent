"""Tests for ``ados.core.pairing.PairingManager``.

Focus on the persisted state file: it must be written via the shared
atomic helper (sensitive data, 0o600 mode), and the manager must round
trip generated codes / claim / unpair through that file.
"""

from __future__ import annotations

import json
import os
import stat
from pathlib import Path

import pytest

from ados.core.pairing import PairingManager


@pytest.fixture
def state_file(tmp_path: Path) -> Path:
    return tmp_path / "pairing.json"


def test_generate_code_persists_with_0o600_mode(state_file: Path) -> None:
    mgr = PairingManager(state_path=str(state_file))
    code = mgr.get_or_create_code()
    assert state_file.is_file()
    assert stat.S_IMODE(os.stat(state_file).st_mode) == 0o600
    on_disk = json.loads(state_file.read_text())
    assert on_disk["pairing_code"] == code


def test_claim_persists_with_0o600_mode(state_file: Path) -> None:
    mgr = PairingManager(state_path=str(state_file))
    mgr.get_or_create_code()
    mgr.claim("user-1")
    assert stat.S_IMODE(os.stat(state_file).st_mode) == 0o600
    on_disk = json.loads(state_file.read_text())
    assert on_disk["paired"] is True
    assert on_disk["owner_id"] == "user-1"
    assert on_disk["api_key"].startswith("ados_")


def test_unpair_persists_with_0o600_mode(state_file: Path) -> None:
    mgr = PairingManager(state_path=str(state_file))
    mgr.get_or_create_code()
    mgr.claim("user-1")
    mgr.unpair()
    assert stat.S_IMODE(os.stat(state_file).st_mode) == 0o600
    on_disk = json.loads(state_file.read_text())
    assert on_disk == {}


def test_state_round_trips_after_reinit(state_file: Path) -> None:
    mgr = PairingManager(state_path=str(state_file))
    mgr.get_or_create_code()
    key = mgr.claim("user-2")
    # Re-instantiate from the on-disk state.
    mgr2 = PairingManager(state_path=str(state_file))
    assert mgr2.is_paired
    assert mgr2.api_key == key
    assert mgr2.owner_id == "user-2"


def test_no_temp_files_left_after_save(state_file: Path) -> None:
    mgr = PairingManager(state_path=str(state_file))
    mgr.get_or_create_code()
    leftovers = [p for p in state_file.parent.iterdir() if p.suffix == ".tmp"]
    assert leftovers == []


def test_code_expires_at_none_when_no_code_active(state_file: Path) -> None:
    mgr = PairingManager(state_path=str(state_file))
    # Fresh manager, nothing on disk → no code yet → no expiry.
    assert mgr.code_expires_at() is None


def test_code_expires_at_returns_creation_plus_ttl(state_file: Path) -> None:
    from ados.core.pairing import CODE_TTL

    mgr = PairingManager(state_path=str(state_file))
    mgr.get_or_create_code()
    created_at = mgr._state["code_created_at"]
    exp = mgr.code_expires_at()
    assert exp == int(created_at) + CODE_TTL


def test_code_expires_at_set_code_path(state_file: Path) -> None:
    from ados.core.pairing import CODE_TTL

    mgr = PairingManager(state_path=str(state_file))
    mgr.set_code("ABC234")
    created_at = mgr._state["code_created_at"]
    exp = mgr.code_expires_at()
    assert exp == int(created_at) + CODE_TTL


def test_code_expires_at_clears_after_claim(state_file: Path) -> None:
    """`claim()` removes `code_created_at` from state, so expiry reads None."""
    mgr = PairingManager(state_path=str(state_file))
    mgr.get_or_create_code()
    assert mgr.code_expires_at() is not None
    mgr.claim("user-1")
    assert mgr.code_expires_at() is None
