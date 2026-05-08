"""Tests for the Pair-drone drilldown detail page."""

from __future__ import annotations

from typing import Any

import pytest
import structlog
from PIL import Image

from ados.services.ui.pages import PageContext, PageNavigator
from ados.services.ui.pages.details.pair_drone import PairDroneDetailPage
from ados.services.ui.theme import DARK
from ados.services.ui.touch.events import TouchGesture


class _Resp:
    def __init__(self, status_code: int, payload: Any) -> None:
        self.status_code = status_code
        self._payload = payload

    def json(self) -> Any:
        return self._payload


class _StubClient:
    def __init__(self, pair_status: dict[str, Any] | None = None) -> None:
        self.gets: list[str] = []
        self.posts: list[tuple[str, Any]] = []
        self._pair_status = pair_status or {}

    async def get(self, url: str, *, timeout: float = 1.5, **_: Any) -> _Resp:
        self.gets.append(url)
        if url.endswith("/api/wfb/pair"):
            return _Resp(200, dict(self._pair_status))
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
        logger=structlog.get_logger("test.pair"),
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
async def test_paired_view_renders_unpair_button() -> None:
    state = {
        "paired_drone": {
            "device_id": "drone-AABBCC",
            "key_fingerprint": "Z" * 24,
            "paired_at_seconds": 720,
            "paired_at": 1_700_000_000.0,
        },
    }
    client = _StubClient()
    ctx = _ctx(state, client)
    page = PairDroneDetailPage()
    await page.on_enter(ctx)
    img = await page.render(ctx)
    assert img.size == (480, 244)
    zone_ids = {z.id for z in page.hit_zones(ctx)}
    assert "details.back" in zone_ids
    assert "pair.unpair" in zone_ids
    assert "pair.open_window" not in zone_ids


@pytest.mark.asyncio
async def test_unpaired_view_renders_open_button_and_qr() -> None:
    state = {
        "paired_drone": {},
        "pairing": {"code": "7YTFC7"},
        "cloud": {"pair_url": "altnautica.com/command?pair=7YTFC7"},
    }
    client = _StubClient()
    ctx = _ctx(state, client)
    page = PairDroneDetailPage()
    await page.on_enter(ctx)
    img = await page.render(ctx)
    assert img.size == (480, 244)
    zone_ids = {z.id for z in page.hit_zones(ctx)}
    assert "pair.open_window" in zone_ids
    assert "pair.unpair" not in zone_ids


@pytest.mark.asyncio
async def test_unpair_tap_pushes_confirm_then_posts_unpair() -> None:
    state = {
        "paired_drone": {
            "device_id": "drone-AABBCC",
            "key_fingerprint": "Z" * 24,
            "paired_at_seconds": 60,
        },
    }
    client = _StubClient()
    ctx = _ctx(state, client)
    page = PairDroneDetailPage()
    await page.on_enter(ctx)
    await page.render(ctx)
    zone = next(z for z in page.hit_zones(ctx) if z.id == "pair.unpair")
    await page.on_touch(ctx, zone, _tap(zone.x + 10, zone.y + 10))
    # Confirm dialog should be on top of the modal stack.
    assert ctx.navigator.modal_stack
    dialog = ctx.navigator.modal_stack[-1]
    assert dialog.id == "modal.confirm"
    # Tap confirm.
    await dialog.render(ctx)
    confirm_zone = next(z for z in dialog.hit_zones(ctx) if z.id == "confirm.ok")
    await dialog.on_touch(
        ctx, confirm_zone, _tap(confirm_zone.x + 10, confirm_zone.y + 10),
    )
    # Unpair endpoint should have been hit.
    assert any(url.endswith("/api/wfb/pair/unpair") for url, _ in client.posts)


@pytest.mark.asyncio
async def test_open_window_tap_posts_local_bind() -> None:
    state = {
        "paired_drone": {},
        "pairing": {"code": "7YTFC7"},
    }
    client = _StubClient()
    ctx = _ctx(state, client)
    page = PairDroneDetailPage()
    await page.on_enter(ctx)
    await page.render(ctx)
    zone = next(z for z in page.hit_zones(ctx) if z.id == "pair.open_window")
    await page.on_touch(ctx, zone, _tap(zone.x + 10, zone.y + 10))
    assert any(url.endswith("/api/v1/pair/local-bind") for url, _ in client.posts)


@pytest.mark.asyncio
async def test_active_window_hides_open_button() -> None:
    state = {
        "paired_drone": {},
        "pairing": {
            "code": "7YTFC7",
            "window": {"active": True, "remaining_seconds": 180},
        },
    }
    client = _StubClient()
    ctx = _ctx(state, client)
    page = PairDroneDetailPage()
    await page.on_enter(ctx)
    await page.render(ctx)
    zone_ids = {z.id for z in page.hit_zones(ctx)}
    assert "pair.open_window" not in zone_ids


@pytest.mark.asyncio
async def test_back_zone_pops_modal() -> None:
    state = {"paired_drone": {}}
    client = _StubClient()
    ctx = _ctx(state, client)
    page = PairDroneDetailPage()
    await ctx.navigator.push_modal(page, ctx=ctx)
    assert ctx.navigator.modal_stack
    back = next(z for z in page.hit_zones(ctx) if z.id == "details.back")
    await page.on_touch(ctx, back, _tap(24, 24))
    assert not ctx.navigator.modal_stack
