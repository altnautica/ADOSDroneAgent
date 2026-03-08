"""Tests for WFB-ng key management."""

from __future__ import annotations

import os
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from ados.services.wfb.key_mgr import (
    WFB_KEY_SIZE,
    generate_key_pair,
    get_key_paths,
    key_exists,
    load_key,
)


def test_key_exists_false(tmp_path: Path):
    assert key_exists(str(tmp_path)) is False


def test_key_exists_partial(tmp_path: Path):
    (tmp_path / "tx.key").write_bytes(os.urandom(32))
    assert key_exists(str(tmp_path)) is False


def test_key_exists_true(tmp_path: Path):
    (tmp_path / "tx.key").write_bytes(os.urandom(32))
    (tmp_path / "rx.key").write_bytes(os.urandom(32))
    assert key_exists(str(tmp_path)) is True


def test_load_key_success(tmp_path: Path):
    key_data = os.urandom(32)
    key_path = tmp_path / "test.key"
    key_path.write_bytes(key_data)

    loaded = load_key(str(key_path))
    assert loaded == key_data


def test_load_key_not_found():
    with pytest.raises(FileNotFoundError):
        load_key("/nonexistent/path/key.file")


def test_load_key_empty(tmp_path: Path):
    key_path = tmp_path / "empty.key"
    key_path.write_bytes(b"")

    with pytest.raises(ValueError, match="empty"):
        load_key(str(key_path))


def test_get_key_paths_default():
    tx, rx = get_key_paths()
    assert tx == "/etc/ados/wfb/tx.key"
    assert rx == "/etc/ados/wfb/rx.key"


def test_get_key_paths_custom():
    tx, rx = get_key_paths("/custom/dir")
    assert tx == "/custom/dir/tx.key"
    assert rx == "/custom/dir/rx.key"


def test_generate_with_cryptography_fallback(tmp_path: Path):
    """When wfb_keygen is not found, should fall back to Python crypto."""
    key_dir = tmp_path / "keys"

    with patch("ados.services.wfb.key_mgr._generate_with_wfb_keygen") as mock_keygen:
        mock_keygen.side_effect = FileNotFoundError("wfb_keygen not found")
        tx_path, rx_path = generate_key_pair(str(key_dir))

    assert Path(tx_path).is_file()
    assert Path(rx_path).is_file()
    assert len(Path(tx_path).read_bytes()) == WFB_KEY_SIZE
    assert len(Path(rx_path).read_bytes()) == WFB_KEY_SIZE


def test_generate_with_wfb_keygen(tmp_path: Path):
    """When wfb_keygen succeeds, should use its output."""
    key_dir = tmp_path / "keys"
    key_dir.mkdir()

    # Simulate wfb_keygen creating gs.key and drone.key
    with patch("ados.services.wfb.key_mgr.subprocess") as mock_sub:
        mock_result = MagicMock()
        mock_result.returncode = 0
        mock_sub.run.return_value = mock_result

        # Create fake keygen output files
        (key_dir / "gs.key").write_bytes(os.urandom(32))
        (key_dir / "drone.key").write_bytes(os.urandom(32))

        tx_path, rx_path = generate_key_pair(str(key_dir))

    assert Path(tx_path).is_file()
    assert Path(rx_path).is_file()


def test_generate_creates_directory(tmp_path: Path):
    """generate_key_pair should create the output directory if it doesn't exist."""
    key_dir = tmp_path / "new" / "nested" / "keys"
    assert not key_dir.exists()

    with patch("ados.services.wfb.key_mgr._generate_with_wfb_keygen") as mock_keygen:
        mock_keygen.side_effect = FileNotFoundError("wfb_keygen not found")
        tx_path, rx_path = generate_key_pair(str(key_dir))

    assert key_dir.exists()
    assert Path(tx_path).is_file()
