"""Tests for the Uplink drilldown detail page."""

from __future__ import annotations

from typing import Any

import pytest
import structlog
from PIL import Image

from ados.services.ui.pages import PageContext, PageNavigator
from ados.services.ui.pages.details.uplink import UplinkDetailPage
from ados.services.ui.theme import DARK
from ados.services.ui.touch.events import TouchGesture


class _Resp:
    def __init__(self, status_code: int, payload: Any) -> None:
        self.status_code = status_code
        self._payload = payload

    def json(self) -> Any:
        return self._payload


class _StubClient:
    def __init__(self, modem_payload: dict[str, Any]) -> None:
        self.gets: list[str] = []
        self._modem = modem_payload

    async def get(self, url: str, *, timeout: float = 1.5, **_: Any) -> _Resp:
        self.gets.append(url)
        if url.endswith("/api/v1/dashboard/snapshot"):
            return _Resp(
                200,
                {
                    "cloud": {
                        "mqtt_state": "connected",
                        "http_state": "connected",
                        "rtt_ms": 18,
                        "drone_id": "drone-AB",
                        "pairing_code": "",
                    }
                },
            )
        if url.endswith("/api/v1/ground-station/modem-status"):
            return _Resp(200, dict(self._modem))
        return _Resp(404, {})


class _StubDashboard:
    id = "dashboard"
    refresh_hz = 5.0
    async def on_enter(self, ctx: PageContext) -> None: ...
    async def on_leave(self, ctx: PageContext) -> None: ...
    async def render(self, ctx: PageContext) -> Image.Image:
        return Image.new("RGB", (480, 244), ctx.palette.bg_primary)
    def hit_zones(self, ctx: PageContext) -> list[Any]: return []
    async def on_touch(self, ctx: PageContext, zone: Any, g: TouchGesture) -> None: ...


def _ctx(state: dict, http: Any) -> PageContext:
    nav = PageNavigator(registry={"dashboard": _StubDashboard()})
    return PageContext(
        state=state,
        palette=DARK,
        hostname="groundnode",
        http=http,
        framebuffer=None,
        navigator=nav,
        logger=structlog.get_logger("test.uplink"),
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
async def test_uplink_modem_present_renders_signal_bars_and_band() -> None:
    state: dict = {}
    client = _StubClient(
        {
            "present": True,
            "rsrp_dbm": -92,
            "rsrq_db": -10,
            "sinr_db": 12,
            "rssi_dbm": -68,
            "band": "B40",
            "operator": "carrier",
            "ip": "10.1.2.3",
            "tech": "lte",
        }
    )
    page = UplinkDetailPage()
    ctx = _ctx(state, client)
    await page.on_enter(ctx)
    img = await page.render(ctx)
    assert img.size == (480, 244)
    # Both endpoints fetched.
    assert any(g.endswith("/api/v1/dashboard/snapshot") for g in client.gets)
    assert any(g.endswith("/api/v1/ground-station/modem-status") for g in client.gets)


@pytest.mark.asyncio
async def test_uplink_modem_absent_falls_back_to_wifi_block() -> None:
    state = {
        "network": {
            "wifi_client": {
                "connected": True,
                "ssid": "groundnet",
                "signal_dbm": -45,
            }
        }
    }
    client = _StubClient({"present": False, "reason": "no_modem"})
    page = UplinkDetailPage()
    ctx = _ctx(state, client)
    await page.on_enter(ctx)
    img = await page.render(ctx)
    assert img.size == (480, 244)


@pytest.mark.asyncio
async def test_uplink_modem_absent_no_wifi_renders_no_wan_uplink() -> None:
    client = _StubClient(
        {"present": False, "reason": "modemmanager_not_installed"},
    )
    page = UplinkDetailPage()
    ctx = _ctx({}, client)
    await page.on_enter(ctx)
    img = await page.render(ctx)
    assert img.size == (480, 244)


@pytest.mark.asyncio
async def test_uplink_back_pops_modal() -> None:
    page = UplinkDetailPage()
    client = _StubClient({"present": False, "reason": "no_modem"})
    ctx = _ctx({}, client)
    await ctx.navigator.push_modal(page, ctx=ctx)
    assert ctx.navigator.modal_stack
    back = next(z for z in page.hit_zones(ctx) if z.id == "details.back")
    await page.on_touch(ctx, back, _tap(24, 24))
    assert not ctx.navigator.modal_stack
