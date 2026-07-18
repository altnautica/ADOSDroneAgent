"""Tests for board-sidecar persistence.

The native control surface reads ``/run/ados/board.json`` for the board's NPU
capability and the perception tier; ``persist_board_sidecar`` is the writer that
keeps that sidecar honest (without it, a real NPU board reports npu_tops:0).
"""

from __future__ import annotations

import json

from ados.hal.detect import BoardInfo, persist_board_sidecar


def _npu_board() -> BoardInfo:
    return BoardInfo(
        name="Radxa ROCK 5C Lite (RK3582)",
        model="rock-5c-lite",
        tier=4,
        ram_mb=16000,
        cpu_cores=8,
        npu_tops=6.0,
    )


def test_persist_writes_npu_capability(tmp_path, monkeypatch):
    import ados.core.paths as paths

    target = tmp_path / "run" / "board.json"
    monkeypatch.setattr(paths, "BOARD_JSON", target)

    assert persist_board_sidecar(_npu_board()) is True
    assert target.is_file()
    data = json.loads(target.read_text())
    assert data["npu_tops"] == 6.0
    assert data["has_accelerator"] is True
    assert data["name"] == "Radxa ROCK 5C Lite (RK3582)"


def test_persist_no_accelerator_board(tmp_path, monkeypatch):
    import ados.core.paths as paths

    target = tmp_path / "board.json"
    monkeypatch.setattr(paths, "BOARD_JSON", target)

    board = BoardInfo(
        name="Raspberry Pi 4 Model B",
        model="rpi4b",
        tier=3,
        ram_mb=4096,
        cpu_cores=4,
    )
    assert persist_board_sidecar(board) is True
    data = json.loads(target.read_text())
    assert data["npu_tops"] == 0.0
    assert data["has_accelerator"] is False


def test_persist_is_atomic_overwrite(tmp_path, monkeypatch):
    import ados.core.paths as paths

    target = tmp_path / "board.json"
    monkeypatch.setattr(paths, "BOARD_JSON", target)

    target.write_text('{"stale": true}')
    assert persist_board_sidecar(_npu_board()) is True
    data = json.loads(target.read_text())
    assert "stale" not in data
    assert data["has_accelerator"] is True
