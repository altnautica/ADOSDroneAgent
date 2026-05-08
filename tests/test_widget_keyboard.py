"""Tests for the KeyboardModal widget."""

from __future__ import annotations

import pytest
import structlog
from PIL import Image

from ados.services.ui.pages import PageContext, PageNavigator
from ados.services.ui.theme import DARK
from ados.services.ui.touch.events import TouchGesture
from ados.services.ui.widgets.onscreen_keyboard import KeyboardModal


def _ctx(navigator: PageNavigator) -> PageContext:
    return PageContext(
        state={},
        palette=DARK,
        hostname="groundnode",
        http=None,
        framebuffer=None,
        navigator=navigator,
        logger=structlog.get_logger("test.kb"),
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


def _long_press(x: int, y: int) -> TouchGesture:
    return TouchGesture(
        kind="long_press",
        start_x=x,
        start_y=y,
        end_x=x,
        end_y=y,
        start_t_ms=0,
        end_t_ms=600,
        duration_ms=600,
        direction=None,
        velocity_px_per_s=0.0,
        samples=((x, y, 0),),
    )


def _key_zone(page: KeyboardModal, ctx: PageContext, label: str):
    """Find a top-three-row key by label."""
    for z in page.hit_zones(ctx):
        if z.id.startswith("kb.key:") and z.id.endswith(f":{label}"):
            return z
    raise AssertionError(f"key {label!r} not found in zones")


def _fn_zone(page: KeyboardModal, ctx: PageContext, label: str):
    for z in page.hit_zones(ctx):
        if z.id == f"kb.fn:{label}":
            return z
    raise AssertionError(f"fn key {label!r} not found in zones")


@pytest.mark.asyncio
async def test_render_returns_480x244() -> None:
    async def _save(v: str) -> None: ...

    page = KeyboardModal(title="SSID", on_save=_save)
    img = await page.render(_ctx(PageNavigator()))
    assert isinstance(img, Image.Image)
    assert img.size == (480, 244)


@pytest.mark.asyncio
async def test_typing_appends_letter() -> None:
    async def _save(v: str) -> None: ...

    page = KeyboardModal(title="SSID", on_save=_save)
    nav = PageNavigator()
    ctx = _ctx(nav)
    z = _key_zone(page, ctx, "a")
    await page.on_touch(ctx, z, _tap(z.x + 5, z.y + 5))
    assert page._value == "a"


@pytest.mark.asyncio
async def test_backspace_deletes_last_char() -> None:
    async def _save(v: str) -> None: ...

    page = KeyboardModal(title="SSID", initial="hello", on_save=_save)
    nav = PageNavigator()
    ctx = _ctx(nav)
    z = _fn_zone(page, ctx, "BKSP")
    await page.on_touch(ctx, z, _tap(z.x + 5, z.y + 5))
    assert page._value == "hell"


@pytest.mark.asyncio
async def test_backspace_long_press_clears_value() -> None:
    async def _save(v: str) -> None: ...

    page = KeyboardModal(title="SSID", initial="hello", on_save=_save)
    nav = PageNavigator()
    ctx = _ctx(nav)
    z = _fn_zone(page, ctx, "BKSP")
    await page.on_touch(ctx, z, _long_press(z.x + 5, z.y + 5))
    assert page._value == ""


@pytest.mark.asyncio
async def test_shift_toggles_letter_case() -> None:
    async def _save(v: str) -> None: ...

    page = KeyboardModal(title="SSID", on_save=_save)
    nav = PageNavigator()
    ctx = _ctx(nav)
    shift = _fn_zone(page, ctx, "SHIFT")
    await page.on_touch(ctx, shift, _tap(shift.x + 5, shift.y + 5))
    # After shift, the upper-case keys should be present in the zone set.
    z = _key_zone(page, ctx, "A")
    assert z is not None


@pytest.mark.asyncio
async def test_save_button_fires_on_save_and_pops() -> None:
    saved: list[str] = []

    async def _save(v: str) -> None:
        saved.append(v)

    page = KeyboardModal(title="SSID", initial="MyAP", on_save=_save)
    nav = PageNavigator()
    ctx = _ctx(nav)
    await nav.push_modal(page, ctx=ctx)
    save = _fn_zone(page, ctx, "SAVE")
    await page.on_touch(ctx, save, _tap(save.x + 10, save.y + 10))
    assert saved == ["MyAP"]
    assert not nav.modal_stack


@pytest.mark.asyncio
async def test_masked_render_shows_dots() -> None:
    async def _save(v: str) -> None: ...

    page = KeyboardModal(
        title="Password",
        initial="secret",
        masked=True,
        on_save=_save,
    )
    img = await page.render(_ctx(PageNavigator()))
    assert isinstance(img, Image.Image)
    # Just confirm value was preserved (rendered as bullets, but state same).
    assert page._value == "secret"
