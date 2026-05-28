"""Tests for the LCD-state retry helper used by the cloud heartbeat.

The OLED service writes ``/run/ados/lcd-state.json`` atomically
(tmpfile + rename), but a reader that catches the inode mid-rename
can still see an empty file. The cloud heartbeat builder used to
swallow that race silently, dropping ``lcdActivePage`` from the
heartbeat payload entirely. This test pins the small retry loop in
``ados.services.cloud.__main__._read_json_with_retry`` to its
contract: try up to N times with a short sleep between attempts,
return None only when the file is genuinely missing or remains
unreadable after the retries are exhausted.
"""

from __future__ import annotations

from pathlib import Path

from ados.services.cloud.__main__ import (
    _read_json_with_retry,
    _read_lcd_state_blob,
)


def test_returns_none_when_file_absent(tmp_path: Path):
    target = tmp_path / "absent.json"
    assert _read_json_with_retry(target) is None


def test_returns_dict_when_populated(tmp_path: Path):
    target = tmp_path / "lcd-state.json"
    target.write_text('{"active_page_id": "dashboard"}')
    out = _read_json_with_retry(target)
    assert out == {"active_page_id": "dashboard"}


def test_retries_on_empty_file_then_succeeds(tmp_path: Path, monkeypatch):
    """Empty on the first read, populated on the second — retry wins."""
    target = tmp_path / "lcd-state.json"
    target.write_text("")  # empty initial content
    call_count = {"reads": 0}

    def flaky_read_text(self, *args, **kwargs):
        call_count["reads"] += 1
        if call_count["reads"] == 1:
            return ""
        # Subsequent reads see the "fully written" content.
        return '{"active_page_id": "dashboard", "modal_stack": []}'

    monkeypatch.setattr(Path, "read_text", flaky_read_text)
    out = _read_json_with_retry(target)
    assert out is not None
    assert out["active_page_id"] == "dashboard"
    assert call_count["reads"] >= 2


def test_retries_on_partial_json_then_succeeds(tmp_path: Path, monkeypatch):
    """Malformed first read, valid second read — retry wins."""
    target = tmp_path / "lcd-state.json"
    target.write_text("placeholder")
    call_count = {"reads": 0}

    def flaky_read_text(self, *args, **kwargs):
        call_count["reads"] += 1
        if call_count["reads"] == 1:
            return "{not json"
        return '{"active_page_id": "settings", "modal_stack": ["modal-a"]}'

    monkeypatch.setattr(Path, "read_text", flaky_read_text)
    out = _read_json_with_retry(target)
    assert out is not None
    assert out["active_page_id"] == "settings"
    assert out["modal_stack"] == ["modal-a"]


def test_returns_none_after_all_retries_exhausted(tmp_path: Path, monkeypatch):
    """All N attempts see an empty file -> give up and return None."""
    target = tmp_path / "lcd-state.json"
    target.write_text("placeholder")
    call_count = {"reads": 0}

    def always_empty(self, *args, **kwargs):
        call_count["reads"] += 1
        return ""

    monkeypatch.setattr(Path, "read_text", always_empty)
    out = _read_json_with_retry(target, attempts=3)
    assert out is None
    # All 3 attempts consumed.
    assert call_count["reads"] == 3


def test_returns_none_on_non_dict_payload(tmp_path: Path):
    target = tmp_path / "lcd-state.json"
    target.write_text("[1, 2, 3]")  # valid JSON but not a dict
    assert _read_json_with_retry(target) is None


def test_retry_attempts_bounded_to_one_minimum(tmp_path: Path):
    """attempts=0 still tries once."""
    target = tmp_path / "lcd-state.json"
    target.write_text('{"x": 1}')
    out = _read_json_with_retry(target, attempts=0)
    assert out == {"x": 1}


def test_lcd_state_blob_uses_canonical_path(tmp_path: Path, monkeypatch):
    """``_read_lcd_state_blob`` reads /run/ados/lcd-state.json."""
    fake_path = tmp_path / "lcd-state.json"
    fake_path.write_text('{"active_page_id": "video", "modal_stack": []}')
    # Patch the LCD_STATE_PATH the function imports.
    import ados.core.paths as _paths

    monkeypatch.setattr(_paths, "LCD_STATE_PATH", fake_path)
    out = _read_lcd_state_blob()
    assert out is not None
    assert out["active_page_id"] == "video"


def test_lcd_state_blob_returns_none_when_missing(tmp_path: Path, monkeypatch):
    fake_path = tmp_path / "absent-lcd-state.json"
    import ados.core.paths as _paths

    monkeypatch.setattr(_paths, "LCD_STATE_PATH", fake_path)
    assert _read_lcd_state_blob() is None
