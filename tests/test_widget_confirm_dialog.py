"""Tests for the ConfirmDialog widget."""

from __future__ import annotations

import pytest
import structlog
from PIL import Image

from ados.services.ui.pages import PageContext, PageNavigator
from ados.services.ui.theme import DARK
from ados.services.ui.touch.events import TouchGesture
from ados.services.ui.widgets.confirm_dialog import ConfirmDialog


def _ctx(navigator: PageNavigator) -> PageContext:
    return PageContext(
        state={},
        palette=DARK,
        hostname="groundnode",
        http=None,
        framebuffer=None,
        navigator=navigator,
        logger=structlog.get_logger("test.confirm"),
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
    fired: list[bool] = []

    async def _on_confirm() -> None:
        fired.append(True)

    page = ConfirmDialog(
        "Reboot now",
        "Reboot the agent now?",
        on_confirm=_on_confirm,
    )
    img = await page.render(_ctx(PageNavigator()))
    assert isinstance(img, Image.Image)
    assert img.size == (480, 244)


@pytest.mark.asyncio
async def test_confirm_fires_action_and_pops() -> None:
    fired: list[bool] = []

    async def _on_confirm() -> None:
        fired.append(True)

    page = ConfirmDialog(
        "Reboot now",
        "Reboot the agent now?",
        on_confirm=_on_confirm,
    )
    nav = PageNavigator()
    ctx = _ctx(nav)
    await nav.push_modal(page, ctx=ctx)
    ok = next(z for z in page.hit_zones(ctx) if z.id == "confirm.ok")
    await page.on_touch(ctx, ok, _tap(ok.x + 10, ok.y + 10))
    assert fired == [True]
    assert not nav.modal_stack


@pytest.mark.asyncio
async def test_cancel_does_not_fire_action() -> None:
    fired: list[bool] = []

    async def _on_confirm() -> None:
        fired.append(True)

    page = ConfirmDialog(
        "Reboot now",
        "Reboot the agent now?",
        on_confirm=_on_confirm,
    )
    nav = PageNavigator()
    ctx = _ctx(nav)
    await nav.push_modal(page, ctx=ctx)
    cancel = next(z for z in page.hit_zones(ctx) if z.id == "confirm.cancel")
    await page.on_touch(ctx, cancel, _tap(cancel.x + 10, cancel.y + 10))
    assert fired == []
    assert not nav.modal_stack


def _has_color(img: Image.Image, color: tuple[int, int, int]) -> bool:
    flat = img.getcolors(maxcolors=480 * 244)
    return any(c == color for _, c in (flat or []))


@pytest.mark.asyncio
async def test_destructive_variant_uses_status_error_color() -> None:
    async def _on_confirm() -> None: ...

    page = ConfirmDialog(
        "Factory reset",
        "Erase all settings?",
        confirm_label="Erase",
        confirm_destructive=True,
        on_confirm=_on_confirm,
    )
    img = await page.render(_ctx(PageNavigator()))
    assert _has_color(img, DARK.status_error)
