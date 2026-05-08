"""Atomic file-write helpers.

Use these whenever a file is read after a power-loss reboot. The write
sequence is: tempfile in the same directory, write, fsync the data fd,
``os.replace`` onto the target, fsync the parent directory. Without the
parent-dir fsync, the rename can be lost on crash even though the data
fd was synced.
"""
from __future__ import annotations

import json
import os
import tempfile
from pathlib import Path
from typing import Any


def _fsync_parent_dir(path: Path) -> None:
    parent = path.parent
    try:
        dir_fd = os.open(parent, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
    except OSError:
        return
    try:
        os.fsync(dir_fd)
    except OSError:
        pass
    finally:
        os.close(dir_fd)


def atomic_write_bytes(path: Path | str, data: bytes, *, mode: int = 0o600) -> None:
    """Atomically write *data* to *path* with fsync semantics."""
    target = Path(path)
    target.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp = tempfile.mkstemp(prefix=target.name + ".", suffix=".tmp", dir=target.parent)
    try:
        try:
            with os.fdopen(fd, "wb") as fh:
                fh.write(data)
                fh.flush()
                os.fsync(fh.fileno())
        except BaseException:
            try:
                os.close(fd)
            except OSError:
                pass
            raise
        os.chmod(tmp, mode)
        os.replace(tmp, target)
        _fsync_parent_dir(target)
        tmp = None  # consumed
    finally:
        if tmp is not None:
            try:
                os.unlink(tmp)
            except OSError:
                pass


def atomic_write_text(path: Path | str, text: str, *, mode: int = 0o600, encoding: str = "utf-8") -> None:
    atomic_write_bytes(path, text.encode(encoding), mode=mode)


def atomic_write_json(
    path: Path | str,
    obj: Any,
    *,
    mode: int = 0o600,
    indent: int | None = 2,
    sort_keys: bool = False,
) -> None:
    payload = json.dumps(obj, indent=indent, sort_keys=sort_keys).encode("utf-8")
    atomic_write_bytes(path, payload, mode=mode)
