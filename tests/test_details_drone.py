"""Tests for the Drone drilldown detail page (paired + unpaired)."""

from __future__ import annotations

from typing import Any

import pytest
import structlog
from PIL import Image

from ados.services.ui.pages import PageContext, PageNavigator
from ados.services.ui.pages.details.drone import DroneDetailPage
from ados.services.ui.theme import DARK
from ados.services.ui.touch.events import TouchGesture


class _Resp:
    def __init__(self, status_code: int, payload: Any) -> None:
        self.status_code = status_code
        self._payload = payload

    def json(self) -> Any:
        return self._payload


class _StubClient:
    def __init__(self, snapshot: dict[str, Any] | None = None) -> None:
        self.gets: list[str] = []
        self.posts: list[tuple[str, Any]] = []
        self._snapshot = snapshot or {
            "fc": {
                "vehicle": "quadcopter",
                "mode": "STAB",
                "armed": False,
                "battery": {"voltage": 12.4, "remaining": 73},
                "gps": {"fix_type": 3, "satellites_visible": 11},
            }
        }

    async def get(self, url: str, *, timeout: float = 1.5, **_: Any) -> _Resp:
        self.gets.append(url)
        if url.endswith("/api/v1/dashboard/snapshot"):
            return _Resp(200, self._snapshot)
        return _Resp(404, {})

    async def post(
        self,
        url: str,
        *,
        json: Any | None = None,
        timeout: float = 2.0,
    ) -> _Resp:
        self.posts.append((url, json))
        return _Resp(200, {"ok": True})


class _StubDashboard:
    id = "dashboard"
    refresh_hz = 5.0

    async def on_enter(self, ctx: PageContext) -> None: ...
    async def on_leave(self, ctx: PageContext) -> None: ...
    async def render(self, ctx: PageContext) -> Image.Image:
        return Image.new("RGB", (480, 244), ctx.palette.bg_primary)
    def hit_zones(self, ctx: PageContext) -> list[Any]: return []
    async def on_touch(self, ctx: PageContext, zone: Any, g: TouchGesture) -> None: ...


def _ctx(state: dict, http: Any | None) -> PageContext:
    nav = PageNavigator(registry={"dashboard": _StubDashboard()})
    return PageContext(
        state=state,
        palette=DARK,
        hostname="groundnode",
        http=http,
        framebuffer=None,
        navigator=nav,
        logger=structlog.get_logger("test.drone"),
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
async def test_drone_detail_paired_renders_battery_grid() -> None:
    state = {
        "paired_drone": {
            "device_id": "drone-AABBCC",
            "key_fingerprint": "Z" * 16,
            "paired_at_seconds": 360,
        }
    }
    client = _StubClient()
    ctx = _ctx(state, client)
    page = DroneDetailPage()
    await page.on_enter(ctx)
    img = await page.render(ctx)
    assert img.size == (480, 244)
    # Snapshot fetched.
    assert any(p.endswith("/api/v1/dashboard/snapshot") for p in client.gets)
    # When paired, the open-pairing zone is NOT exposed.
    zones = {z.id for z in page.hit_zones(ctx)}
    assert "details.back" in zones
    assert "drone.open_pairing" not in zones


@pytest.mark.asyncio
async def test_drone_detail_unpaired_renders_qr_and_pairing_button() -> None:
    state = {
        "paired_drone": {},
        "cloud": {"pair_code": "7YTFC7", "pair_url": "altnautica.com/command"},
        "pairing": {"code": "7YTFC7"},
    }
    client = _StubClient(snapshot={"fc": {}})
    ctx = _ctx(state, client)
    page = DroneDetailPage()
    await page.on_enter(ctx)
    img = await page.render(ctx)
    assert img.size == (480, 244)
    zones = {z.id for z in page.hit_zones(ctx)}
    assert "drone.open_pairing" in zones
    # Tap the pairing button — POST should fire.
    open_zone = next(z for z in page.hit_zones(ctx) if z.id == "drone.open_pairing")
    await page.on_touch(ctx, open_zone, _tap(240, 208))
    assert any(url.endswith("/api/v1/pair/local-bind") for url, _ in client.posts)


@pytest.mark.asyncio
async def test_drone_detail_back_zone_pops_modal() -> None:
    state = {"paired_drone": {}}
    client = _StubClient(snapshot={"fc": {}})
    ctx = _ctx(state, client)
    page = DroneDetailPage()
    await ctx.navigator.push_modal(page, ctx=ctx)
    assert ctx.navigator.modal_stack
    back = next(z for z in page.hit_zones(ctx) if z.id == "details.back")
    await page.on_touch(ctx, back, _tap(24, 24))
    assert not ctx.navigator.modal_stack
