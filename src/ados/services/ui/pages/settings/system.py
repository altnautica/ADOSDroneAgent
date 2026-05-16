"""System-level settings rows: theme, log level, reboot, factory reset, about.

These are the rows that don't fit cleanly under the radio or
network groupings. They flip global agent state, surface confirm
dialogs for destructive actions, or push the read-only About detail
page.
"""

from __future__ import annotations

from typing import TYPE_CHECKING

from ados.services.ui.pages.base import PageContext
from ados.services.ui.widgets import (
    ConfirmDialog,
    EnumPickerModal,
)

from ._common import _post_apply
from ._row import Row

if TYPE_CHECKING:
    from .page import SettingsPage


async def _theme_enum(
    page: "SettingsPage", ctx: PageContext, row: Row,
) -> None:
    options = [("dark", "Dark"), ("light", "Light")]
    current = page._value_for(row.id, ctx) or "dark"

    async def _save(value: str) -> None:
        await _post_apply(ctx, {"ui": {"theme": value}})
        await page._refresh(ctx, force=True)

    await ctx.navigator.push_modal(
        EnumPickerModal(
            title="Theme",
            options=options,
            current=str(current),
            on_save=_save,
        ),
        ctx=ctx,
    )


async def _log_level_enum(
    page: "SettingsPage", ctx: PageContext, row: Row,
) -> None:
    options = [
        ("debug", "Debug"),
        ("info", "Info"),
        ("warning", "Warning"),
        ("error", "Error"),
    ]
    current = page._value_for(row.id, ctx) or "info"

    async def _save(value: str) -> None:
        await _post_apply(ctx, {"advanced": {"log_level": value}})
        page._bump_pending_reboot(ctx)
        await page._refresh(ctx, force=True)

    await ctx.navigator.push_modal(
        EnumPickerModal(
            title="Log level",
            options=options,
            current=str(current),
            on_save=_save,
        ),
        ctx=ctx,
    )


async def _reboot_action(
    page: "SettingsPage", ctx: PageContext, row: Row,
) -> None:
    await page._show_reboot_dialog(ctx)


async def _factory_reset_action(
    page: "SettingsPage", ctx: PageContext, row: Row,
) -> None:
    async def _on_confirm() -> None:
        client = ctx.http
        if client is None:
            return
        try:
            await client.post("/api/v1/setup/reset", timeout=2.0)
            await client.post("/api/v1/setup/reboot", timeout=2.0)
            ctx.state["pending_reboot_count"] = 0
        except Exception as exc:  # noqa: BLE001
            ctx.logger.warning("settings_factory_reset_failed", error=str(exc))

    await ctx.navigator.push_modal(
        ConfirmDialog(
            "Factory reset",
            (
                "Wipes all setup state, pairings, and operator-set config. "
                "The agent reboots after the reset."
            ),
            confirm_label="Erase",
            confirm_destructive=True,
            on_confirm=_on_confirm,
        ),
        ctx=ctx,
    )


async def _about_drilldown(
    page: "SettingsPage", ctx: PageContext, row: Row,
) -> None:
    from ados.services.ui.pages.details.about import AboutDetailPage

    await ctx.navigator.push_modal(AboutDetailPage(), ctx=ctx)


__all__ = [
    "_theme_enum",
    "_log_level_enum",
    "_reboot_action",
    "_factory_reset_action",
    "_about_drilldown",
]
