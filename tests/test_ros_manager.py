"""Tests for the ROS manager foxglove bind probe."""

from __future__ import annotations

import socket
from unittest import mock

from ados.core.config import ADOSConfig
from ados.services.ros_manager import RosManager


def _fresh_manager() -> RosManager:
    return RosManager(ADOSConfig())


def test_foxglove_bind_failed_default_false() -> None:
    """A fresh manager reports no bind failure until probed."""
    rm = _fresh_manager()
    assert rm.foxglove_bind_failed() is False


def test_foxglove_bind_probe_flips_on_connect_error() -> None:
    """connect_ex returns nonzero → flag flips True."""
    rm = _fresh_manager()

    fake_sock = mock.MagicMock()
    fake_sock.connect_ex.return_value = 111  # ECONNREFUSED-style
    fake_sock.__enter__.return_value = fake_sock
    fake_sock.__exit__.return_value = False

    with mock.patch.object(socket, "socket", return_value=fake_sock):
        rm._probe_foxglove_bind()
    assert rm.foxglove_bind_failed() is True


def test_foxglove_bind_probe_stays_false_on_success() -> None:
    """connect_ex returns 0 → flag stays False."""
    rm = _fresh_manager()

    fake_sock = mock.MagicMock()
    fake_sock.connect_ex.return_value = 0
    fake_sock.__enter__.return_value = fake_sock
    fake_sock.__exit__.return_value = False

    with mock.patch.object(socket, "socket", return_value=fake_sock):
        rm._probe_foxglove_bind()
    assert rm.foxglove_bind_failed() is False


def test_foxglove_bind_probe_handles_oserror() -> None:
    """A raised OSError during probe → flag flips True."""
    rm = _fresh_manager()

    with mock.patch.object(socket, "socket", side_effect=OSError("boom")):
        rm._probe_foxglove_bind()
    assert rm.foxglove_bind_failed() is True
