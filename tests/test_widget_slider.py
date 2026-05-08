"""Tests for the SliderModal widget."""

from __future__ import annotations

import pytest
import structlog
from PIL import Image

from ados.services.ui.pages import PageContext, PageNavigator
from ados.services.ui.theme import DARK
from ados.services.ui.touch.events import TouchGesture
from ados.services.ui.widgets.slider import SliderModal


def _ctx(navigator: PageNavigator) -> PageContext:
    return PageContext(
        state={},
        palette=DARK,
        hostname="groundnode",
        http=None,
        framebuffer=None,
        navigator=navigator,
        logger=structlog.get_logger("test.slider"),
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


def _drag(x0: int, y0: int, x1: int, y1: int) -> TouchGesture:
    return TouchGesture(
        kind="drag",
        start_x=x0,
        start_y=y0,
        end_x=x1,
        end_y=y1,
        start_t_ms=0,
        end_t_ms=400,
        duration_ms=400,
        direction="right" if x1 >= x0 else "left",
        velocity_px_per_s=400.0,
        samples=((x0, y0, 0), (x1, y1, 400)),
    )


@pytest.mark.asyncio
async def test_render_returns_480x244() -> None:
    async def _save(v: int) -> None: ...

    page = SliderModal(
        title="TX power",
        min_val=1,
        max_val=15,
        step=1,
        current=5,
        unit="dBm",
        on_save=_save,
    )
    img = await page.render(_ctx(PageNavigator()))
    assert isinstance(img, Image.Image)
    assert img.size == (480, 244)


@pytest.mark.asyncio
async def test_minus_step_decrements() -> None:
    async def _save(v: int) -> None: ...

    page = SliderModal(
        title="TX power",
        min_val=1,
        max_val=15,
        step=1,
        current=10,
        unit="dBm",
        on_save=_save,
    )
    nav = PageNavigator()
    ctx = _ctx(nav)
    minus = next(z for z in page.hit_zones(ctx) if z.id == "slider.minus")
    await page.on_touch(ctx, minus, _tap(minus.x + 10, minus.y + 10))
    assert page._value == 9


@pytest.mark.asyncio
async def test_plus_step_increments() -> None:
    async def _save(v: int) -> None: ...

    page = SliderModal(
        title="TX power",
        min_val=1,
        max_val=15,
        step=1,
        current=10,
        unit="dBm",
        on_save=_save,
    )
    nav = PageNavigator()
    ctx = _ctx(nav)
    plus = next(z for z in page.hit_zones(ctx) if z.id == "slider.plus")
    await page.on_touch(ctx, plus, _tap(plus.x + 10, plus.y + 10))
    assert page._value == 11


@pytest.mark.asyncio
async def test_drag_to_set_uses_release_x() -> None:
    async def _save(v: int) -> None: ...

    page = SliderModal(
        title="TX power",
        min_val=1,
        max_val=15,
        step=1,
        current=5,
        unit="dBm",
        on_save=_save,
    )
    nav = PageNavigator()
    ctx = _ctx(nav)
    track = next(z for z in page.hit_zones(ctx) if z.id == "slider.track")
    drag = _drag(track.x + 20, track.y + 10, track.x + track.w - 20, track.y + 10)
    await page.on_touch(ctx, track, drag)
    # Released near the right end → near max.
    assert page._value >= 13


@pytest.mark.asyncio
async def test_save_button_commits_and_pops_modal() -> None:
    saved: list[int] = []

    async def _save(v: int) -> None:
        saved.append(v)

    page = SliderModal(
        title="TX power",
        min_val=1,
        max_val=15,
        step=1,
        current=7,
        unit="dBm",
        on_save=_save,
    )
    nav = PageNavigator()
    ctx = _ctx(nav)
    await nav.push_modal(page, ctx=ctx)
    save_zone = next(z for z in page.hit_zones(ctx) if z.id == "slider.save")
    await page.on_touch(ctx, save_zone, _tap(save_zone.x + 50, save_zone.y + 10))
    assert saved == [7]
    assert not nav.modal_stack
