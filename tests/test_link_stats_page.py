"""Tests for the Link Stats page (replaces the old More tab).

The page reads four upstream sources at ~1 Hz inside ``render()``: the
agent's ``/api/wfb`` endpoint, the local mediamtx control-plane at
9997, and two JSON files under ``/run/ados/``. The tests stub all
four so a CI runner can validate render behavior without an agent or
mediamtx running.
"""

from __future__ import annotations

import json
from typing import Any

import pytest
import structlog
from PIL import Image

from ados.services.ui.pages import PageContext, PageNavigator
from ados.services.ui.pages.link_stats import LinkStatsPage
from ados.services.ui.theme import DARK
from ados.services.ui.touch.events import TouchGesture


class _Resp:
    def __init__(self, status_code: int, payload: Any) -> None:
        self.status_code = status_code
        self._payload = payload

    def json(self) -> Any:
        return self._payload


class _StubClient:
    """Routes /api/wfb to a wfb stats payload and the absolute mediamtx
    URL to a path-state payload. Both can be empty / configurable per
    test by overriding the class attributes."""

    wfb_payload: dict = {}
    mtx_payload: dict = {}
    wfb_status: int = 200
    mtx_status: int = 200

    def __init__(self) -> None:
        self.gets: list[str] = []

    async def get(
        self, url: str, *, timeout: float = 1.5, **_: Any
    ) -> _Resp:
        self.gets.append(url)
        if url == "/api/wfb":
            return _Resp(self.wfb_status, self.wfb_payload)
        if url.endswith("/v3/paths/get/main"):
            return _Resp(self.mtx_status, self.mtx_payload)
        return _Resp(404, {})


def _ctx(navigator: PageNavigator, http: Any | None) -> PageContext:
    return PageContext(
        state={},
        palette=DARK,
        hostname="skynode",
        http=http,
        framebuffer=None,
        navigator=navigator,
        logger=structlog.get_logger("test.link_stats"),
    )


def _tap(x: int, y: int) -> TouchGesture:
    return TouchGesture(
        kind="tap",
        start_x=x,
        start_y=y,
        end_x=x,
        end_y=y,
        start_t_ms=0,
        end_t_ms=10,
        duration_ms=10,
        direction=None,
        velocity_px_per_s=0.0,
        samples=((x, y, 0),),
    )


@pytest.mark.asyncio
async def test_link_stats_renders_at_480x244() -> None:
    page = LinkStatsPage()
    nav = PageNavigator(registry={"link_stats": page})
    client = _StubClient()
    client.wfb_payload = {
        "state": "connected",
        "rssi_dbm": -52.0,
        "channel": 149,
        "bitrate_kbps": 4800,
        "packets_received": 12450,
        "packets_lost": 150,
        "loss_percent": 1.2,
        "fec_recovered": 8,
        "fec_failed": 0,
    }
    client.mtx_payload = {"ready": True, "inboundBytes": 1234567}
    ctx = _ctx(nav, client)
    await page.on_enter(ctx)
    img = await page.render(ctx)
    assert img.size == (480, 244)
    # Page polled both expected sources on enter
    assert "/api/wfb" in client.gets
    assert any(
        u.endswith("/v3/paths/get/main") for u in client.gets
    )


@pytest.mark.asyncio
async def test_link_stats_handles_missing_data() -> None:
    """Render must succeed even when every upstream returns nothing."""
    page = LinkStatsPage()
    nav = PageNavigator(registry={"link_stats": page})
    client = _StubClient()
    client.wfb_payload = {}
    client.mtx_payload = {}
    ctx = _ctx(nav, client)
    await page.on_enter(ctx)
    img = await page.render(ctx)
    assert img.size == (480, 244)


@pytest.mark.asyncio
async def test_link_stats_records_rssi_history() -> None:
    page = LinkStatsPage()
    nav = PageNavigator(registry={"link_stats": page})
    client = _StubClient()
    client.wfb_payload = {"rssi_dbm": -55, "state": "connected"}
    ctx = _ctx(nav, client)
    await page.on_enter(ctx)
    # Force several refreshes by spacing the gate manually.
    for delta in (1.5, 2.5, 3.5):
        page._last_refresh_at -= delta  # bypass throttle
        await page._refresh(ctx)
    history = list(page._rssi_history)
    real = [s for s in history if s is not None]
    assert len(real) >= 1
    assert all(isinstance(s, float) for s in real)


@pytest.mark.asyncio
async def test_link_stats_hit_zone_covers_body() -> None:
    page = LinkStatsPage()
    nav = PageNavigator(registry={"link_stats": page})
    ctx = _ctx(nav, _StubClient())
    zones = page.hit_zones(ctx)
    assert len(zones) == 1
    z = zones[0]
    assert z.id == "link_stats:body"
    assert z.x == 0 and z.y == 0
    assert z.w == 480 and z.h == 244


@pytest.mark.asyncio
async def test_link_stats_no_op_on_touch() -> None:
    """Tap must not raise; current behavior is logging-only."""
    page = LinkStatsPage()
    nav = PageNavigator(registry={"link_stats": page})
    ctx = _ctx(nav, _StubClient())
    zones = page.hit_zones(ctx)
    await page.on_touch(ctx, zones[0], _tap(240, 122))
    # No assertion — just must not raise


@pytest.mark.asyncio
async def test_link_stats_inbound_rate_delta() -> None:
    """Two successive refreshes with mediamtx inboundBytes delta should
    populate the kbps gauge."""
    page = LinkStatsPage()
    nav = PageNavigator(registry={"link_stats": page})
    client = _StubClient()
    client.wfb_payload = {"state": "connected"}
    ctx = _ctx(nav, client)

    client.mtx_payload = {"ready": True, "inboundBytes": 0}
    await page.on_enter(ctx)
    page._last_refresh_at -= 2.0  # bypass throttle for second refresh
    # Synthesise time elapsed by manually setting the _mtx_inbound_at
    # back so the next refresh computes a positive dt.
    page._mtx_inbound_at -= 1.0
    client.mtx_payload = {"ready": True, "inboundBytes": 100_000}
    await page._refresh(ctx)
    # 100_000 bytes / ~1 s = ~100 kB/s = ~800 kbps — exact value depends
    # on monotonic clock skew during the test, so assert positivity not
    # exact magnitude.
    assert page._mtx_inbound_kbps is not None
    assert page._mtx_inbound_kbps > 0
