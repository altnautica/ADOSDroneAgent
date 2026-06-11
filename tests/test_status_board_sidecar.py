"""The status route's board-sidecar write-once bridge.

The native control surface reads the full HAL board dict from a sidecar rather
than running HAL detect itself, so the FastAPI status route persists the detected
board there once per process. These tests cover the write-once guard, the
empty-board no-op, and the atomic write content.
"""

from __future__ import annotations

import json

import ados.api.routes.status as status


def _reset_guard():
    status._board_persisted = False


def test_persist_board_writes_the_dict_sorted(tmp_path, monkeypatch):
    _reset_guard()
    target = tmp_path / "board.json"
    monkeypatch.setattr(status, "BOARD_JSON", target)

    board = {"soc": "BCM2711", "name": "rpi4b", "ram_mb": 4096}
    status._persist_board_once(board)

    assert target.exists()
    written = json.loads(target.read_text())
    assert written == board
    # Keys are sorted on disk (stable output for a byte-comparing reader).
    assert list(written.keys()) == sorted(board.keys())
    assert status._board_persisted is True


def test_persist_board_is_write_once(tmp_path, monkeypatch):
    _reset_guard()
    target = tmp_path / "board.json"
    monkeypatch.setattr(status, "BOARD_JSON", target)

    status._persist_board_once({"name": "first"})
    # A second call with different content must NOT overwrite (board is static
    # per boot; the guard skips after the first successful write).
    status._persist_board_once({"name": "second"})

    assert json.loads(target.read_text()) == {"name": "first"}


def test_persist_board_empty_is_a_noop(tmp_path, monkeypatch):
    _reset_guard()
    target = tmp_path / "board.json"
    monkeypatch.setattr(status, "BOARD_JSON", target)

    # An empty board (a detect that raised) writes nothing and leaves the guard
    # unset so a later non-empty detect can still persist.
    status._persist_board_once({})
    assert not target.exists()
    assert status._board_persisted is False


def test_persist_board_swallows_write_errors(tmp_path, monkeypatch):
    _reset_guard()
    # Point the sidecar at a path whose parent is a file, so mkdir/write fails;
    # the helper must swallow the OSError and leave the guard unset.
    blocker = tmp_path / "blocker"
    blocker.write_text("not a dir")
    monkeypatch.setattr(status, "BOARD_JSON", blocker / "board.json")

    status._persist_board_once({"name": "rpi4b"})  # must not raise
    assert status._board_persisted is False
