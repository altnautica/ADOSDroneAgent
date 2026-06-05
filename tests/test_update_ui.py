"""Unit tests for the interactive ``ados update`` renderer (pure functions)."""

from __future__ import annotations

from ados.cli import update_ui as u


def test_status_maps_state_to_phase() -> None:
    m = u._Model(new_version="0.52.1", current_version="0.52.0")
    m.apply_status({"state": "downloading", "download": {"percent": 40}})
    assert m.active == 0
    m.apply_status({"state": "installing", "download": {}})
    assert m.active == 2
    # An earlier phase completing bumps done_through.
    assert m.done_through >= 1
    assert not m.failed


def test_failed_state_flags_failure() -> None:
    m = u._Model(new_version="0.52.1", current_version="0.52.0")
    m.apply_status({"state": "failed"})
    assert m.failed


def test_block_rows_are_exact_width_without_color() -> None:
    th = u._Theme(color=False, ascii=False)
    m = u._Model(
        new_version="0.52.1",
        current_version="0.52.0",
        active=0,
        download={"percent": 63, "speed_bps": 3.2e6},
    )
    for width in (40, 58, 64):
        lines = u._block_lines(th, m, 2, width)
        assert len(lines) == len(u.PHASES) + 2
        for line in lines:
            assert len(line) == width, f"line not width {width}: {line!r}"


def test_ascii_tier_uses_ascii_glyphs() -> None:
    th = u._Theme(color=False, ascii=True)
    assert th.glyph_ok() == "+"
    assert th.glyph_fail() == "x"
    assert th.spinner(0) == "-"
    assert th.box()[4] == "-"


def test_bar_fills_and_clamps() -> None:
    th = u._Theme(color=False, ascii=True)
    assert th and "#" not in u._bar(th, 0)
    assert u._bar(th, 100) == "[########]"
    # Out-of-range percent clamps rather than overflowing.
    assert len(u._bar(th, 150)) == len(u._bar(th, 50))
