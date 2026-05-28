"""Tests for the EnumPickerModal widget."""

from __future__ import annotations

import pytest
import structlog
from PIL import Image

from ados.services.ui.pages import PageContext, PageNavigator
from ados.services.ui.theme import DARK
from ados.services.ui.touch.events import TouchGesture
from ados.services.ui.widgets.enum_picker import EnumPickerModal


def _ctx(navigator: PageNavigator) -> PageContext:
    return PageContext(
        state={},
        palette=DARK,
        hostname="groundnode",
        http=None,
        framebuffer=None,
        navigator=navigator,
        logger=structlog.get_logger("test.enum"),
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
async def test_render_returns_480x244() -> None:
    saved: list[str] = []

    async def _save(value: str) -> None:
        saved.append(value)

    page = EnumPickerModal(
        title="Channel",
        options=[("36", "36"), ("149", "149")],
        current="149",
        on_save=_save,
    )
    nav = PageNavigator()
    img = await page.render(_ctx(nav))
    assert isinstance(img, Image.Image)
    assert img.size == (480, 244)


@pytest.mark.asyncio
async def test_tap_option_fires_on_save_and_pops_modal() -> None:
    saved: list[str] = []

    async def _save(value: str) -> None:
        saved.append(value)

    page = EnumPickerModal(
        title="Channel",
        options=[("36", "36"), ("149", "149")],
        current=None,
        on_save=_save,
    )
    nav = PageNavigator()
    ctx = _ctx(nav)
    await nav.push_modal(page, ctx=ctx)
    zone = next(z for z in page.hit_zones(ctx) if z.id == "enum.option:149")
    await page.on_touch(ctx, zone, _tap(zone.x + 10, zone.y + 10))
    assert saved == ["149"]
    assert not nav.modal_stack


@pytest.mark.asyncio
async def test_back_zone_pops_without_save() -> None:
    saved: list[str] = []

    async def _save(value: str) -> None:
        saved.append(value)

    page = EnumPickerModal(
        title="Channel",
        options=[("36", "36"), ("149", "149")],
        current=None,
        on_save=_save,
    )
    nav = PageNavigator()
    ctx = _ctx(nav)
    await nav.push_modal(page, ctx=ctx)
    back = next(z for z in page.hit_zones(ctx) if z.id == "details.back")
    await page.on_touch(ctx, back, _tap(20, 20))
    assert saved == []
    assert not nav.modal_stack


@pytest.mark.asyncio
async def test_scroll_changes_visible_window() -> None:
    async def _save(value: str) -> None:
        ...

    options = [(str(i), str(i)) for i in range(20)]
    page = EnumPickerModal(
        title="Big list",
        options=options,
        current=None,
        on_save=_save,
    )
    nav = PageNavigator()
    ctx = _ctx(nav)
    base_zones = {z.id for z in page.hit_zones(ctx)}
    page._y_offset = 100
    new_zones = {z.id for z in page.hit_zones(ctx)}
    assert base_zones != new_zones
