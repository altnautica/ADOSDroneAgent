"""Tests for the Channel Hops LCD page.

The page reads /run/ados/hop-supervisor.json on every refresh tick and
renders a step-after chart of channel-vs-time with scatter markers per
hop. These tests stub the JSON file path and verify the empty +
populated rendering paths without needing the supervisor running.
"""

from __future__ import annotations

import json
from typing import Any

import pytest
import structlog

from ados.services.ui.pages import PageContext, PageNavigator
from ados.services.ui.pages.channel_hops import ChannelHopsPage
from ados.services.ui.theme import DARK


class _Resp:
    def __init__(self, status_code: int, payload: Any) -> None:
        self.status_code = status_code
        self._payload = payload

    def json(self) -> Any:
        return self._payload


class _StubClient:
    """Returns a configurable /api/wfb payload for current-channel
    discovery. Other endpoints fall through to 404."""

    wfb_payload: dict = {"channel": 149}
    wfb_status: int = 200

    def __init__(self) -> None:
        self.gets: list[str] = []

    async def get(self, url: str, *, timeout: float = 1.5, **_: Any) -> _Resp:
        self.gets.append(url)
        if url == "/api/wfb":
            return _Resp(self.wfb_status, self.wfb_payload)
        return _Resp(404, {})


def _ctx(navigator: PageNavigator, http: Any | None) -> PageContext:
    return PageContext(
        state={},
        palette=DARK,
        hostname="groundnode",
        http=http,
        framebuffer=None,
        navigator=navigator,
        logger=structlog.get_logger("test.channel_hops"),
    )


@pytest.fixture
def fixture_state_file(tmp_path, monkeypatch):
    """Redirect the page to a temp JSON file so we can control history."""
    path = tmp_path / "hop-supervisor.json"
    monkeypatch.setattr(
        "ados.services.ui.pages.channel_hops.HOP_SUPERVISOR_JSON", path
    )
    return path


def _write_state(path, *, history: list[dict], band: str = "u-nii-1") -> None:
    path.write_text(
        json.dumps(
            {
                "enabled": True,
                "band": band,
                "hop_period_seconds": 60,
                "history": history,
                "last_hop_at": history[-1]["at"] if history else 0.0,
            }
        )
    )


@pytest.mark.asyncio
async def test_empty_state_renders_placeholder(fixture_state_file):
    _write_state(fixture_state_file, history=[])
    page = ChannelHopsPage()
    navigator = PageNavigator()
    navigator.register(page)
    ctx = _ctx(navigator, _StubClient())

    img = await page.render(ctx)
    assert img.size == (480, 244)
    # Page survives empty history without throwing. Verify the radio
    # channel was discovered from the stubbed /api/wfb response so
    # the placeholder can show "current channel 149".
    assert page._radio_channel == 149


@pytest.mark.asyncio
async def test_populated_history_renders_chart(fixture_state_file):
    import time

    now = time.time()
    history = [
        {"at": now - 240, "from": 149, "to": 36, "trigger": "periodic", "ok": True},
        {"at": now - 180, "from": 36, "to": 44, "trigger": "periodic", "ok": True},
        {"at": now - 120, "from": 44, "to": 48, "trigger": "reactive", "ok": True},
        {"at": now - 60, "from": 48, "to": 40, "trigger": "reactive", "ok": False},
        {"at": now, "from": 40, "to": 44, "trigger": "periodic", "ok": True},
    ]
    _write_state(fixture_state_file, history=history)

    page = ChannelHopsPage()
    navigator = PageNavigator()
    navigator.register(page)
    ctx = _ctx(navigator, _StubClient())

    img = await page.render(ctx)
    assert img.size == (480, 244)
    assert len(page._history()) == 5


@pytest.mark.asyncio
async def test_marker_color_mapping():
    page = ChannelHopsPage()
    palette = DARK
    # periodic + ok -> green
    assert page._marker_color(palette, "periodic", True) == palette.status_success
    # reactive + ok -> amber
    assert page._marker_color(palette, "reactive", True) == palette.status_warning
    # any + failed -> red
    assert page._marker_color(palette, "periodic", False) == palette.status_error
    assert page._marker_color(palette, "reactive", False) == palette.status_error


@pytest.mark.asyncio
async def test_history_drops_malformed_entries(fixture_state_file):
    import time

    now = time.time()
    # Mix of valid + malformed entries; the page should only count valid.
    history = [
        {"at": now - 60, "from": 149, "to": 44, "trigger": "periodic", "ok": True},
        {"at": now - 30},  # missing fields
        "not even a dict",  # garbage
        {"at": now, "from": 44, "to": 36, "trigger": "reactive", "ok": True},
    ]
    fixture_state_file.write_text(
        json.dumps(
            {"enabled": True, "band": "u-nii-1", "history": history}
        )
    )

    page = ChannelHopsPage()
    navigator = PageNavigator()
    navigator.register(page)
    ctx = _ctx(navigator, _StubClient())

    await page.render(ctx)
    # 2 valid + 1 malformed-dict-without-keys + 1 non-dict; only the 2
    # full entries should survive the filter.
    assert len(page._history()) == 2


@pytest.mark.asyncio
async def test_missing_state_file_renders_empty(fixture_state_file):
    # File doesn't exist; page should fall back to empty state.
    assert not fixture_state_file.exists()
    page = ChannelHopsPage()
    navigator = PageNavigator()
    navigator.register(page)
    ctx = _ctx(navigator, _StubClient())
    img = await page.render(ctx)
    assert img.size == (480, 244)
    assert page._history() == []
