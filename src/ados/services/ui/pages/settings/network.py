"""Network-related settings rows: hotspot SSID, hotspot toggle, Wi-Fi client.

Each handler pushes the appropriate modal (keyboard / confirm dialog),
issues the matching REST call on save, and forces a snapshot refresh so
the row redraws immediately.
"""

from __future__ import annotations

from typing import TYPE_CHECKING

from ados.services.ui.pages.base import PageContext
from ados.services.ui.widgets import (
    ConfirmDialog,
    KeyboardModal,
)

from ._common import _post_apply
from ._row import Row

if TYPE_CHECKING:
    from .page import SettingsPage


async def _wifi_hotspot_drilldown(
    page: SettingsPage, ctx: PageContext, row: Row,
) -> None:
    initial = page._value_for(row.id, ctx) or ""

    async def _save(value: str) -> None:
        await _post_apply(ctx, {"network": {"hotspot_enabled": True}})
        # Hotspot SSID write would land in a future apply schema slot.
        await page._refresh(ctx, force=True)

    await ctx.navigator.push_modal(
        KeyboardModal(
            title="Hotspot SSID",
            initial=str(initial),
            placeholder="ADOS-AP",
            on_save=_save,
        ),
        ctx=ctx,
    )


async def _hotspot_toggle(
    page: SettingsPage, ctx: PageContext, row: Row,
) -> None:
    current = bool(page._value_for(row.id, ctx))
    new_value = not current
    if not new_value:
        # Disabling the hotspot is destructive on a board reachable
        # only via that AP — confirm before committing.
        async def _on_confirm() -> None:
            await _post_apply(ctx, {"network": {"hotspot_enabled": False}})
            await page._refresh(ctx, force=True)

        await ctx.navigator.push_modal(
            ConfirmDialog(
                "Disable hotspot?",
                "Devices connected only via this hotspot will lose access until you re-enable it.",
                confirm_label="Disable",
                confirm_destructive=True,
                on_confirm=_on_confirm,
            ),
            ctx=ctx,
        )
        return
    await _post_apply(ctx, {"network": {"hotspot_enabled": True}})
    await page._refresh(ctx, force=True)


async def _wifi_client_drilldown(
    page: SettingsPage, ctx: PageContext, row: Row,
) -> None:
    current = page._value_for(row.id, ctx) or ""

    async def _save_ssid(ssid: str) -> None:
        await _post_apply(ctx, {"network": {"wifi_ssid": ssid}})
        await page._refresh(ctx, force=True)
        # Now ask for the password.

        async def _save_pw(pw: str) -> None:
            await _post_apply(
                ctx,
                {"network": {"wifi_password": pw}},
            )
            await page._refresh(ctx, force=True)

        await ctx.navigator.push_modal(
            KeyboardModal(
                title="Wi-Fi password",
                initial="",
                masked=True,
                on_save=_save_pw,
            ),
            ctx=ctx,
        )

    await ctx.navigator.push_modal(
        KeyboardModal(
            title="Wi-Fi SSID",
            initial=str(current) if current != "Not configured" else "",
            placeholder="MyNetwork",
            on_save=_save_ssid,
        ),
        ctx=ctx,
    )


__all__ = [
    "_wifi_hotspot_drilldown",
    "_hotspot_toggle",
    "_wifi_client_drilldown",
]
