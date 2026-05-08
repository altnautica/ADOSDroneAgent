"""Tests for the /etc/ados/display.conf rotation helper."""

from __future__ import annotations

import pytest

from ados.services.ui.display_conf import (
    ALLOWED_ROTATIONS,
    read_rotation,
    write_rotation,
)


def test_write_then_read_round_trips(tmp_path) -> None:
    target = tmp_path / "display.conf"
    write_rotation(180, path=target)
    assert read_rotation(path=target) == 180


def test_read_returns_zero_when_file_missing(tmp_path) -> None:
    target = tmp_path / "missing.conf"
    assert read_rotation(path=target) == 0


def test_write_preserves_other_keys(tmp_path) -> None:
    target = tmp_path / "display.conf"
    target.write_text(
        "display_id=waveshare35a\nframebuffer_path=/dev/fb1\n"
    )
    write_rotation(90, path=target)
    body = target.read_text()
    assert "display_id=waveshare35a" in body
    assert "framebuffer_path=/dev/fb1" in body
    assert "rotation=90" in body
    assert read_rotation(path=target) == 90


def test_write_rejects_invalid_rotation(tmp_path) -> None:
    target = tmp_path / "display.conf"
    with pytest.raises(ValueError):
        write_rotation(45, path=target)


@pytest.mark.parametrize("value", ALLOWED_ROTATIONS)
def test_write_accepts_each_allowed_value(tmp_path, value) -> None:
    target = tmp_path / "display.conf"
    write_rotation(value, path=target)
    assert read_rotation(path=target) == value


def test_read_returns_zero_for_corrupt_value(tmp_path) -> None:
    target = tmp_path / "display.conf"
    target.write_text("rotation=banana\n")
    assert read_rotation(path=target) == 0


def test_read_returns_zero_for_out_of_range_value(tmp_path) -> None:
    target = tmp_path / "display.conf"
    target.write_text("rotation=720\n")
    assert read_rotation(path=target) == 0
