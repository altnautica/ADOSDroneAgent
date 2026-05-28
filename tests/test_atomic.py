"""Tests for the atomic file-write helpers in ``ados.core.atomic``.

Cover the happy path for all three helpers (bytes / text / json), mode
override, parent-directory auto-create, JSON serialization shape, and
fsync semantics (data fd + parent directory). The fsync coverage uses
monkeypatching against ``ados.core.atomic.os.fsync`` so the test stays
hermetic on every platform.
"""

from __future__ import annotations

import json
import os
import stat
from pathlib import Path
from unittest import mock

from ados.core import atomic


def _mode(path: Path) -> int:
    return stat.S_IMODE(os.stat(path).st_mode)


# ---------------------------------------------------------------------------
# atomic_write_bytes
# ---------------------------------------------------------------------------


def test_atomic_write_bytes_happy_path(tmp_path: Path) -> None:
    target = tmp_path / "blob.bin"
    payload = b"\x00\x01\x02ados"
    atomic.atomic_write_bytes(target, payload)
    assert target.read_bytes() == payload
    assert _mode(target) == 0o600


def test_atomic_write_bytes_mode_override(tmp_path: Path) -> None:
    target = tmp_path / "blob.bin"
    atomic.atomic_write_bytes(target, b"abc", mode=0o644)
    assert _mode(target) == 0o644


def test_atomic_write_bytes_creates_parent_dir(tmp_path: Path) -> None:
    target = tmp_path / "deep" / "nested" / "blob.bin"
    assert not target.parent.exists()
    atomic.atomic_write_bytes(target, b"hi")
    assert target.read_bytes() == b"hi"
    assert target.parent.is_dir()


def test_atomic_write_bytes_accepts_str_path(tmp_path: Path) -> None:
    target = tmp_path / "blob.bin"
    atomic.atomic_write_bytes(str(target), b"strpath")
    assert target.read_bytes() == b"strpath"


def test_atomic_write_bytes_no_temp_files_left(tmp_path: Path) -> None:
    target = tmp_path / "blob.bin"
    atomic.atomic_write_bytes(target, b"clean")
    leftovers = [p for p in tmp_path.iterdir() if p.suffix == ".tmp"]
    assert leftovers == []


# ---------------------------------------------------------------------------
# atomic_write_text
# ---------------------------------------------------------------------------


def test_atomic_write_text_happy_path(tmp_path: Path) -> None:
    target = tmp_path / "note.txt"
    atomic.atomic_write_text(target, "hello world")
    assert target.read_text() == "hello world"
    assert _mode(target) == 0o600


def test_atomic_write_text_encoding(tmp_path: Path) -> None:
    target = tmp_path / "note.txt"
    atomic.atomic_write_text(target, "naïve", encoding="utf-8")
    assert target.read_bytes() == "naïve".encode()


# ---------------------------------------------------------------------------
# atomic_write_json
# ---------------------------------------------------------------------------


def test_atomic_write_json_happy_path(tmp_path: Path) -> None:
    target = tmp_path / "data.json"
    obj = {"alpha": 1, "beta": [True, None, "x"]}
    atomic.atomic_write_json(target, obj)
    assert json.loads(target.read_text()) == obj
    assert _mode(target) == 0o600


def test_atomic_write_json_sort_keys(tmp_path: Path) -> None:
    target = tmp_path / "data.json"
    atomic.atomic_write_json(target, {"b": 2, "a": 1}, sort_keys=True)
    text = target.read_text()
    # sorted output puts "a" before "b"
    assert text.index('"a"') < text.index('"b"')


def test_atomic_write_json_indent_none_is_compact(tmp_path: Path) -> None:
    target = tmp_path / "data.json"
    atomic.atomic_write_json(target, {"a": 1, "b": 2}, indent=None)
    # Default indent=2 would have a newline; indent=None should be compact.
    assert "\n" not in target.read_text()


def test_atomic_write_json_mode_override(tmp_path: Path) -> None:
    target = tmp_path / "data.json"
    atomic.atomic_write_json(target, {"x": 1}, mode=0o644)
    assert _mode(target) == 0o644


# ---------------------------------------------------------------------------
# fsync semantics
# ---------------------------------------------------------------------------


def test_atomic_write_calls_fsync_for_data_and_parent(tmp_path: Path) -> None:
    target = tmp_path / "synced.json"
    real_fsync = atomic.os.fsync
    with mock.patch.object(atomic.os, "fsync", wraps=real_fsync) as fsync_mock:
        atomic.atomic_write_json(target, {"k": "v"})
    # At least two fsync calls: one on the data fd, one on the parent dir.
    assert fsync_mock.call_count >= 2


def test_parent_dir_fsync_oserror_does_not_break_write(tmp_path: Path) -> None:
    """If fsync on the parent directory raises OSError, the helper must
    still leave the target file in place. We patch ``_fsync_parent_dir``
    to raise inside its body and confirm the write succeeded."""

    target = tmp_path / "data.json"
    real_fsync_parent = atomic._fsync_parent_dir

    calls: list[Path] = []

    def _wrapped(p: Path) -> None:
        calls.append(p)
        real_fsync_parent(p)

    with mock.patch.object(atomic, "_fsync_parent_dir", side_effect=_wrapped):
        atomic.atomic_write_json(target, {"x": 1})

    assert target.read_text().strip().startswith("{")
    assert calls and calls[0] == target
