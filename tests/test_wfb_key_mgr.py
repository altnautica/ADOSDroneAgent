"""Tests for WFB-ng key management.

The agent only generates keys via the upstream `wfb_keygen` binary;
the prior 32-byte SHA-256 fallback was removed because wfb-ng
requires the 64-byte libsodium crypto_box keypair format and the
fallback's output silently failed decryption.
"""

from __future__ import annotations

import os
import subprocess
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from ados.services.wfb.key_mgr import (
    WFB_KEY_FILE_BYTES,
    generate_key_pair,
    get_key_paths,
    key_exists,
    load_key,
    read_public_fingerprint,
)


def _write_64b(path: Path, data: bytes | None = None) -> bytes:
    """Helper: write a synthetic 64-byte wfb-ng key file."""
    body = data if data is not None else os.urandom(WFB_KEY_FILE_BYTES)
    assert len(body) == WFB_KEY_FILE_BYTES
    path.write_bytes(body)
    return body


def test_key_exists_false(tmp_path: Path) -> None:
    assert key_exists(str(tmp_path)) is False


def test_key_exists_partial(tmp_path: Path) -> None:
    _write_64b(tmp_path / "tx.key")
    assert key_exists(str(tmp_path)) is False


def test_key_exists_true(tmp_path: Path) -> None:
    _write_64b(tmp_path / "tx.key")
    _write_64b(tmp_path / "rx.key")
    assert key_exists(str(tmp_path)) is True


def test_load_key_success(tmp_path: Path) -> None:
    body = _write_64b(tmp_path / "test.key")
    assert load_key(str(tmp_path / "test.key")) == body


def test_load_key_not_found() -> None:
    with pytest.raises(FileNotFoundError):
        load_key("/nonexistent/path/key.file")


def test_load_key_empty(tmp_path: Path) -> None:
    (tmp_path / "empty.key").write_bytes(b"")
    with pytest.raises(ValueError, match="empty"):
        load_key(str(tmp_path / "empty.key"))


def test_get_key_paths_default() -> None:
    tx, rx = get_key_paths()
    assert tx == "/etc/ados/wfb/tx.key"
    assert rx == "/etc/ados/wfb/rx.key"


def test_get_key_paths_custom() -> None:
    tx, rx = get_key_paths("/custom/dir")
    assert tx == "/custom/dir/tx.key"
    assert rx == "/custom/dir/rx.key"


def test_read_public_fingerprint_returns_16_hex(tmp_path: Path) -> None:
    body = b"\x00" * 32 + b"\x01" * 32
    path = tmp_path / "key"
    path.write_bytes(body)
    fp = read_public_fingerprint(path)
    assert isinstance(fp, str)
    assert len(fp) == 16
    int(fp, 16)


def test_read_public_fingerprint_stable(tmp_path: Path) -> None:
    body = b"\x42" * 32 + b"\x99" * 32
    p1 = tmp_path / "a"
    p2 = tmp_path / "b"
    p1.write_bytes(body)
    p2.write_bytes(body)
    assert read_public_fingerprint(p1) == read_public_fingerprint(p2)


def test_read_public_fingerprint_only_pub_half(tmp_path: Path) -> None:
    """Two keys with the same pub half but different private half should
    produce the same fingerprint — fingerprint is computed over bytes
    32:64 only."""
    pub = b"\xab" * 32
    priv1 = b"\x10" * 32
    priv2 = b"\x20" * 32
    p1 = tmp_path / "a"
    p2 = tmp_path / "b"
    p1.write_bytes(priv1 + pub)
    p2.write_bytes(priv2 + pub)
    assert read_public_fingerprint(p1) == read_public_fingerprint(p2)


def test_read_public_fingerprint_different_pub(tmp_path: Path) -> None:
    p1 = tmp_path / "a"
    p2 = tmp_path / "b"
    p1.write_bytes(b"\x00" * 32 + b"\x01" * 32)
    p2.write_bytes(b"\x00" * 32 + b"\x02" * 32)
    assert read_public_fingerprint(p1) != read_public_fingerprint(p2)


def test_read_public_fingerprint_rejects_wrong_size(tmp_path: Path) -> None:
    path = tmp_path / "short"
    path.write_bytes(b"\x00" * 32)
    with pytest.raises(ValueError, match="32 bytes, expected 64"):
        read_public_fingerprint(path)


def test_read_public_fingerprint_missing_file() -> None:
    with pytest.raises(FileNotFoundError):
        read_public_fingerprint("/nonexistent/key")


def test_generate_with_wfb_keygen_success(tmp_path: Path) -> None:
    """When wfb_keygen exists and produces 64B files, rename to tx/rx."""
    key_dir = tmp_path / "keys"

    def fake_run(cmd, **kwargs):  # noqa: ARG001
        cwd = Path(kwargs["cwd"])
        cwd.mkdir(parents=True, exist_ok=True)
        (cwd / "gs.key").write_bytes(os.urandom(WFB_KEY_FILE_BYTES))
        (cwd / "drone.key").write_bytes(os.urandom(WFB_KEY_FILE_BYTES))
        result = MagicMock()
        result.returncode = 0
        result.stderr = ""
        return result

    with patch("ados.services.wfb.key_mgr.subprocess.run", side_effect=fake_run):
        tx_path, rx_path = generate_key_pair(str(key_dir))

    assert Path(tx_path).is_file()
    assert Path(rx_path).is_file()
    assert Path(tx_path).stat().st_size == WFB_KEY_FILE_BYTES
    assert Path(rx_path).stat().st_size == WFB_KEY_FILE_BYTES


def test_generate_raises_when_wfb_keygen_missing(tmp_path: Path) -> None:
    """No fallback path: wfb_keygen absence is a hard failure."""
    key_dir = tmp_path / "keys"

    with patch(
        "ados.services.wfb.key_mgr.subprocess.run",
        side_effect=FileNotFoundError(),
    ):
        with pytest.raises(FileNotFoundError, match="wfb_keygen"):
            generate_key_pair(str(key_dir))


def test_generate_raises_on_nonzero_exit(tmp_path: Path) -> None:
    key_dir = tmp_path / "keys"

    def fake_run(cmd, **kwargs):  # noqa: ARG001
        result = MagicMock()
        result.returncode = 7
        result.stderr = "boom"
        return result

    with patch("ados.services.wfb.key_mgr.subprocess.run", side_effect=fake_run):
        with pytest.raises(RuntimeError, match="boom"):
            generate_key_pair(str(key_dir))


def test_generate_raises_on_wrong_size(tmp_path: Path) -> None:
    """A wfb_keygen build that produced 32-byte output (the previous bug)
    must be detected and rejected, not silently shipped."""
    key_dir = tmp_path / "keys"

    def fake_run(cmd, **kwargs):  # noqa: ARG001
        cwd = Path(kwargs["cwd"])
        cwd.mkdir(parents=True, exist_ok=True)
        (cwd / "gs.key").write_bytes(os.urandom(32))
        (cwd / "drone.key").write_bytes(os.urandom(32))
        result = MagicMock()
        result.returncode = 0
        result.stderr = ""
        return result

    with patch("ados.services.wfb.key_mgr.subprocess.run", side_effect=fake_run):
        with pytest.raises(RuntimeError, match="size 32"):
            generate_key_pair(str(key_dir))


def test_generate_creates_directory(tmp_path: Path) -> None:
    key_dir = tmp_path / "new" / "nested" / "keys"
    assert not key_dir.exists()

    def fake_run(cmd, **kwargs):  # noqa: ARG001
        cwd = Path(kwargs["cwd"])
        (cwd / "gs.key").write_bytes(os.urandom(WFB_KEY_FILE_BYTES))
        (cwd / "drone.key").write_bytes(os.urandom(WFB_KEY_FILE_BYTES))
        result = MagicMock()
        result.returncode = 0
        result.stderr = ""
        return result

    with patch("ados.services.wfb.key_mgr.subprocess.run", side_effect=fake_run):
        tx_path, rx_path = generate_key_pair(str(key_dir))

    assert key_dir.exists()
    assert Path(tx_path).is_file()
    assert Path(rx_path).is_file()
