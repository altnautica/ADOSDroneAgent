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


def test_get_or_create_code_seeds_both_code_and_pending_key(state_file: Path) -> None:
    """A fresh manager's first code seeds the stable pending API key too.

    The Rust pairing beacon reads ``pending_api_key`` straight from
    ``pairing.json``; it must be present the moment a code exists so the key the
    beacon registers is the same key ``claim()`` later persists (no key drift,
    no permanent 401 after the claim).
    """
    mgr = PairingManager(state_path=str(state_file))
    code = mgr.get_or_create_code()
    on_disk = json.loads(state_file.read_text())
    assert on_disk["pairing_code"] == code
    assert on_disk["pending_api_key"].startswith("ados_")


def test_get_or_create_code_keeps_one_stable_pending_key(state_file: Path) -> None:
    """Re-reading the code does not rotate the pending key."""
    mgr = PairingManager(state_path=str(state_file))
    mgr.get_or_create_code()
    first_key = json.loads(state_file.read_text())["pending_api_key"]
    # A second read returns the same code (within the TTL) and the same key.
    mgr.get_or_create_code()
    second_key = json.loads(state_file.read_text())["pending_api_key"]
    assert first_key == second_key


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


def _write_behind_the_cache(state_file: Path, state: dict, mtime: float) -> None:
    """Another process writes ``state`` within the same mtime tick the reader
    last loaded at, so the reader's mtime check cannot notice it."""
    state_file.write_text(json.dumps(state))
    os.utime(state_file, (mtime, mtime))


def test_a_stale_code_regeneration_never_undoes_a_claim(state_file: Path) -> None:
    """A process whose cached code has expired regenerates it from the state on
    disk under the lock, so a claim another process made is left intact."""
    _write_behind_the_cache(
        state_file, {"pairing_code": "OLD234", "code_created_at": 0}, 1_000.0
    )
    stale = PairingManager(state_path=str(state_file))
    _write_behind_the_cache(
        state_file,
        {"paired": True, "api_key": "ados_live", "owner_id": "u", "paired_at": 1.0},
        1_000.0,
    )
    assert stale.get_or_create_code() == ""
    on_disk = json.loads(state_file.read_text())
    assert on_disk["paired"] is True
    assert on_disk["api_key"] == "ados_live"


def test_a_second_claim_against_a_stale_cache_is_refused(state_file: Path) -> None:
    _write_behind_the_cache(state_file, {"pairing_code": "ABC234"}, 1_000.0)
    stale = PairingManager(state_path=str(state_file))
    _write_behind_the_cache(
        state_file,
        {"paired": True, "api_key": "ados_first", "owner_id": "a", "paired_at": 1.0},
        1_000.0,
    )
    with pytest.raises(ValueError):
        stale.claim("b")
    assert json.loads(state_file.read_text())["api_key"] == "ados_first"


def test_a_writer_never_replaces_an_unreadable_file(state_file: Path) -> None:
    from ados.core.pairing import PairingStateUnreadable

    mgr = PairingManager(state_path=str(state_file))
    state_file.write_text("{not json")
    with pytest.raises(PairingStateUnreadable):
        mgr.claim("u")
    assert state_file.read_text() == "{not json"
