"""The residual API listens only on its internal Unix socket."""

from __future__ import annotations

import socket
import stat

from ados.api import internal_socket


def test_without_an_override_the_listener_is_the_run_dir_socket(
    monkeypatch, unix_socket_dir
) -> None:
    monkeypatch.delenv(internal_socket.API_INTERNAL_SOCKET_ENV, raising=False)
    monkeypatch.setattr(internal_socket, "ADOS_RUN_DIR", unix_socket_dir)

    sock = internal_socket.bind_internal_socket()
    try:
        assert sock.family == socket.AF_UNIX
        assert internal_socket.internal_socket_path() == unix_socket_dir / "api-internal.sock"
        assert (unix_socket_dir / "api-internal.sock").is_socket()
    finally:
        sock.close()


def test_the_socket_is_never_reachable_by_other_users(monkeypatch, unix_socket_dir) -> None:
    path = unix_socket_dir / "nested" / "api.sock"
    monkeypatch.setenv(internal_socket.API_INTERNAL_SOCKET_ENV, str(path))
    # A stale socket file from a previous run must not block the bind.
    path.parent.mkdir()
    path.write_text("stale")

    sock = internal_socket.bind_internal_socket(internal_socket.internal_socket_path())
    try:
        mode = stat.S_IMODE(path.stat().st_mode)
        assert mode & 0o007 == 0, oct(mode)
    finally:
        sock.close()
