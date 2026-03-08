"""Tests for HAL board detection — should work on any platform."""

from __future__ import annotations

import platform

from ados.hal.detect import BoardInfo, detect_board, detect_tier


def test_detect_tier_values():
    """Tier assignment by RAM."""
    assert detect_tier(256) == 1
    assert detect_tier(512) == 2
    assert detect_tier(1024) == 2
    assert detect_tier(2048) == 3
    assert detect_tier(4096) == 3
    assert detect_tier(8192) == 4


def test_detect_board_doesnt_crash():
    """detect_board() should return a BoardInfo on any platform."""
    board = detect_board()
    assert isinstance(board, BoardInfo)
    assert board.name != ""
    assert board.ram_mb > 0
    assert board.cpu_cores >= 1
    assert board.tier >= 1


def test_detect_board_macos_name():
    """On macOS, fallback should say 'macOS (dev)', not 'generic-arm64'."""
    if platform.system() != "Darwin":
        return  # skip on non-macOS
    board = detect_board()
    assert "macOS" in board.name or "generic" in board.name
    assert board.name != "generic-arm64"  # the old broken behavior
