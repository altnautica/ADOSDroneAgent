"""Reader/writer locking for ``/etc/ados/config.yaml``.

The config file is written by several independent processes: the native
pairing and bind flow, the native control surface, the CLI, and the
maintenance migration. The native writers already serialise on
``/run/ados/config.yaml.lock`` (see the ``bind`` module's
``CONFIG_LOCK_PATH``), so this module exists to put the Python side on the
same lock rather than beside it — a lock only one participant takes is not
a lock.

Two postures, deliberately different:

* **Writers must not proceed without the lock.** Every writer performs a
  read-modify-write of a whole mapping. Two of them running unsynchronised
  do not corrupt the file — ``os.replace`` is atomic — they *lose an
  update*, which is worse, because the file carries the radio pairing key,
  the profile and the role, and the loss is silent.
* **Readers must always proceed.** Eleven units call ``load_config()`` on
  their own startup path. If a reader could fail or block indefinitely
  because ``/run`` was not writable or a writer misbehaved, the lock would
  have converted a config-write race into a boot failure. So the read side
  takes the shared lock on a bounded deadline and continues without it on
  expiry.

The bounded wait is implemented by polling ``LOCK_NB`` rather than blocking
in ``LOCK_SH``: a blocking ``flock`` cannot be given a deadline without
signals, and signal-based timeouts are not safe in a process with threads,
which every agent service has.
"""

from __future__ import annotations

import errno
import fcntl
import os
import time
from collections.abc import Iterator
from contextlib import contextmanager
from pathlib import Path

from ados.core.paths import CONFIG_LOCK

# The read path runs on eleven concurrent unit startups. A writer holds the
# lock only for a YAML serialize plus an atomic rename, so a reader that
# waits this long has hit something pathological and is better off reading
# the file — which is itself replaced atomically — than delaying a boot.
READ_LOCK_TIMEOUT_S = 2.0

# The maintenance pass is not on anybody's startup path, so it can afford
# to wait out a real writer.
WRITE_LOCK_TIMEOUT_S = 10.0

_POLL_INTERVAL_S = 0.02


def _open_lock_file(path: Path) -> int | None:
    """Open (creating) the lock file, or ``None`` if that is impossible.

    Returns ``None`` rather than raising for every reason a node might not
    be able to host the lock: ``/run/ados`` absent on a dev box, a
    read-only root, or a non-root process that cannot create the file.
    """
    try:
        parent = path.parent
        if not parent.is_dir():
            os.makedirs(parent, exist_ok=True)
        return os.open(str(path), os.O_CREAT | os.O_RDWR, 0o600)
    except OSError:
        return None


def _acquire(fd: int, operation: int, timeout_s: float) -> bool:
    """Poll ``flock`` in non-blocking mode until ``timeout_s`` elapses."""
    deadline = time.monotonic() + max(timeout_s, 0.0)
    while True:
        try:
            fcntl.flock(fd, operation | fcntl.LOCK_NB)
            return True
        except OSError as exc:
            if exc.errno not in (errno.EACCES, errno.EAGAIN):
                # Not contention — a filesystem that cannot lock (some
                # network mounts) or a bad descriptor. Do not spin.
                return False
        if time.monotonic() >= deadline:
            return False
        time.sleep(_POLL_INTERVAL_S)


@contextmanager
def _config_lock(
    operation: int, timeout_s: float, path: Path | None = None
) -> Iterator[bool]:
    lock_path = path if path is not None else CONFIG_LOCK
    fd = _open_lock_file(lock_path)
    if fd is None:
        yield False
        return
    acquired = _acquire(fd, operation, timeout_s)
    try:
        yield acquired
    finally:
        try:
            if acquired:
                fcntl.flock(fd, fcntl.LOCK_UN)
        finally:
            os.close(fd)


@contextmanager
def shared_config_lock(
    timeout_s: float = READ_LOCK_TIMEOUT_S, path: Path | None = None
) -> Iterator[bool]:
    """Hold a shared lock over a config read, if one can be had.

    Yields whether the lock was acquired. Callers are expected to read
    either way: see the module docstring on why a read never fails for
    want of a lock.
    """
    with _config_lock(fcntl.LOCK_SH, timeout_s, path) as acquired:
        yield acquired


@contextmanager
def exclusive_config_lock(
    timeout_s: float = WRITE_LOCK_TIMEOUT_S, path: Path | None = None
) -> Iterator[bool]:
    """Hold an exclusive lock over a config read-modify-write.

    Yields whether the lock was acquired. A caller that gets ``False``
    must abandon the write; writing anyway is the lost-update hazard this
    lock exists to prevent.
    """
    with _config_lock(fcntl.LOCK_EX, timeout_s, path) as acquired:
        yield acquired
