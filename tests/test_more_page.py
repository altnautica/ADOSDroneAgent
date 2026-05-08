"""Tests for the More page (the "+" tab overflow menu)."""

from __future__ import annotations

from typing import Any

import pytest
import structlog
from PIL import Image

from ados.services.ui.pages import PageContext, PageNavigator
from ados.services.ui.pages.more import MorePage
from ados.services.ui.theme import DARK
from ados.services.ui.touch.events import TouchGesture


class _Resp:
    def __init__(self, status_code: int, payload: Any) -> None:
        self.status_code = status_code
        self._payload = payload

    def json(self) -> Any:
        return self._payload


class _StubClient:
    """Captures every REST call so the tests can assert dispatch."""

    def __init__(self) -> None:
        self.gets: list[str] = []
        self.posts: list[tuple[str, Any]] = []

    async def get(self, url: str, *, timeout: float = 1.5, **_: Any) -> _Resp:
        self.gets.append(url)
        return _Resp(200, {})

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


def _ctx(navigator: PageNavigator, http: Any | None) -> PageContext:
    return PageContext(
        state={},
        palette=DARK,
        hostname="groundnode",
        http=http,
        framebuffer=None,
        navigator=navigator,
        logger=structlog.get_logger("test.more"),
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
async def test_more_page_renders_four_rows() -> None:
    page = MorePage()
    nav = PageNavigator(registry={"dashboard": _StubDashboard(), "more": page})
    ctx = _ctx(nav, _StubClient())
    await page.on_enter(ctx)
    img = await page.render(ctx)
    assert img.size == (480, 244)
    zone_ids = {z.id for z in page.hit_zones(ctx)}
    assert "row:more.row.pair" in zone_ids
    assert "row:more.row.diagnostics" in zone_ids
    assert "row:more.row.restart" in zone_ids
    assert "row:more.row.about" in zone_ids


@pytest.mark.asyncio
async def test_pair_row_pushes_pair_drone_modal() -> None:
    page = MorePage()
    nav = PageNavigator(registry={"dashboard": _StubDashboard(), "more": page})
    ctx = _ctx(nav, _StubClient())
    await page.on_enter(ctx)
    await page.render(ctx)
    zone = next(z for z in page.hit_zones(ctx) if z.id == "row:more.row.pair")
    await page.on_touch(ctx, zone, _tap(zone.x + 10, zone.y + 10))
    assert nav.modal_stack
    assert nav.modal_stack[-1].id == "details.pair_drone"


@pytest.mark.asyncio
async def test_diagnostics_row_pushes_diagnostics_modal() -> None:
    page = MorePage()
    nav = PageNavigator(registry={"dashboard": _StubDashboard(), "more": page})
    ctx = _ctx(nav, _StubClient())
    await page.on_enter(ctx)
    await page.render(ctx)
    zone = next(
        z for z in page.hit_zones(ctx) if z.id == "row:more.row.diagnostics"
    )
    await page.on_touch(ctx, zone, _tap(zone.x + 10, zone.y + 10))
    assert nav.modal_stack
    assert nav.modal_stack[-1].id == "details.diagnostics"


@pytest.mark.asyncio
async def test_restart_row_pushes_confirm_dialog_then_posts() -> None:
    page = MorePage()
    nav = PageNavigator(registry={"dashboard": _StubDashboard(), "more": page})
    client = _StubClient()
    ctx = _ctx(nav, client)
    await page.on_enter(ctx)
    await page.render(ctx)
    zone = next(z for z in page.hit_zones(ctx) if z.id == "row:more.row.restart")
    await page.on_touch(ctx, zone, _tap(zone.x + 10, zone.y + 10))
    # Confirm dialog should be on top.
    assert nav.modal_stack
    dialog = nav.modal_stack[-1]
    assert dialog.id == "modal.confirm"
    # Find the confirm zone and tap it.
    await dialog.render(ctx)
    confirm_zone = next(
        z for z in dialog.hit_zones(ctx) if z.id == "confirm.ok"
    )
    await dialog.on_touch(
        ctx, confirm_zone, _tap(confirm_zone.x + 10, confirm_zone.y + 10),
    )
    # Modal should pop and the POST should fire.
    assert not nav.modal_stack
    assert any(
        url.endswith("/api/v1/system/restart-supervisor")
        for url, _ in client.posts
    )


@pytest.mark.asyncio
async def test_about_row_pushes_about_modal() -> None:
    page = MorePage()
    nav = PageNavigator(registry={"dashboard": _StubDashboard(), "more": page})
    ctx = _ctx(nav, _StubClient())
    await page.on_enter(ctx)
    await page.render(ctx)
    zone = next(z for z in page.hit_zones(ctx) if z.id == "row:more.row.about")
    await page.on_touch(ctx, zone, _tap(zone.x + 10, zone.y + 10))
    assert nav.modal_stack
    assert nav.modal_stack[-1].id == "details.about"
