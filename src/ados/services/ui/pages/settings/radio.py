"""Radio-link settings rows: WFB channel / TX power / MCS / topology / auto-pair / role.

These rows are clustered because they all touch the air-side WFB stack
or the ground-station role selector that sits next to it. A change to
any of them typically requires a wfb-tx restart, which is signalled via
the page's reboot-banner counter.
"""

from __future__ import annotations

from typing import TYPE_CHECKING

from ados.services.ui.pages.base import PageContext
from ados.services.ui.widgets import (
    EnumPickerModal,
    SliderModal,
)

from ._common import _post_apply
from ._row import Row

if TYPE_CHECKING:
    from .page import SettingsPage


async def _channel_enum(
    page: "SettingsPage", ctx: PageContext, row: Row,
) -> None:
    options = [
        ("36", "36 (5180 MHz)"),
        ("48", "48 (5240 MHz)"),
        ("149", "149 (5745 MHz)"),
        ("153", "153 (5765 MHz)"),
        ("157", "157 (5785 MHz)"),
        ("161", "161 (5805 MHz)"),
        ("165", "165 (5825 MHz)"),
    ]
    current = page._wfb.get("channel") if isinstance(page._wfb, dict) else None
    current_str = str(current) if current is not None else None

    async def _save(value: str) -> None:
        client = ctx.http
        if client is not None:
            try:
                await client.post(
                    "/api/wfb/channel",
                    json={"channel": int(value)},
                    timeout=2.0,
                )
            except Exception as exc:  # noqa: BLE001
                ctx.logger.debug("settings_channel_post_failed", error=str(exc))
        # Channel change requires a wfb-tx restart; surface the banner.
        page._bump_pending_reboot(ctx)
        await page._refresh(ctx, force=True)

    await ctx.navigator.push_modal(
        EnumPickerModal(
            title="Channel",
            options=options,
            current=current_str,
            on_save=_save,
        ),
        ctx=ctx,
    )


async def _tx_power_slider(
    page: "SettingsPage", ctx: PageContext, row: Row,
) -> None:
    current = page._wfb.get("tx_power_dbm") if isinstance(page._wfb, dict) else None
    cur_int = int(current) if isinstance(current, (int, float)) else 5

    async def _save(value: int) -> None:
        client = ctx.http
        if client is not None:
            try:
                await client.put(
                    "/api/wfb/tx-power",
                    json={"tx_power_dbm": int(value)},
                    timeout=2.0,
                )
            except Exception as exc:  # noqa: BLE001
                ctx.logger.debug("settings_tx_power_put_failed", error=str(exc))
        await page._refresh(ctx, force=True)

    await ctx.navigator.push_modal(
        SliderModal(
            title="TX power",
            min_val=1,
            max_val=15,
            step=1,
            current=cur_int,
            unit="dBm",
            on_save=_save,
        ),
        ctx=ctx,
    )


async def _mcs_enum(
    page: "SettingsPage", ctx: PageContext, row: Row,
) -> None:
    options = [(str(i), f"MCS {i}") for i in range(8)]
    current = page._wfb.get("mcs_index") if isinstance(page._wfb, dict) else None
    current_str = str(int(current)) if isinstance(current, (int, float)) else None

    async def _save(value: str) -> None:
        await _post_apply(ctx, {"wfb": {"mcs_index": int(value)}})
        page._bump_pending_reboot(ctx)
        await page._refresh(ctx, force=True)

    await ctx.navigator.push_modal(
        EnumPickerModal(
            title="MCS index",
            options=options,
            current=current_str,
            on_save=_save,
        ),
        ctx=ctx,
    )


async def _topology_enum(
    page: "SettingsPage", ctx: PageContext, row: Row,
) -> None:
    options = [
        ("host_vbus", "Host VBUS (USB-A)"),
        ("powered_hub", "Powered USB hub"),
        ("external_5v", "External 5 V rail"),
    ]
    current = page._wfb.get("topology") if isinstance(page._wfb, dict) else None

    async def _save(value: str) -> None:
        await _post_apply(ctx, {"wfb": {"topology": value}})
        page._bump_pending_reboot(ctx)
        await page._refresh(ctx, force=True)

    await ctx.navigator.push_modal(
        EnumPickerModal(
            title="Topology",
            options=options,
            current=str(current) if current is not None else None,
            on_save=_save,
        ),
        ctx=ctx,
    )


async def _auto_pair_toggle(
    page: "SettingsPage", ctx: PageContext, row: Row,
) -> None:
    current = bool(page._value_for(row.id, ctx))
    new_value = not current
    client = ctx.http
    if client is not None:
        try:
            await client.put(
                "/api/wfb/pair/auto-pair",
                json={"enabled": new_value},
                timeout=2.0,
            )
        except Exception as exc:  # noqa: BLE001
            ctx.logger.debug("settings_auto_pair_put_failed", error=str(exc))
    await page._refresh(ctx, force=True)


async def _role_enum(
    page: "SettingsPage", ctx: PageContext, row: Row,
) -> None:
    options = [
        ("direct", "Direct (single node)"),
        ("relay", "Relay"),
        ("receiver", "Receiver"),
    ]
    current = page._value_for(row.id, ctx)

    async def _save(value: str) -> None:
        client = ctx.http
        if client is not None:
            try:
                await client.post(
                    "/api/v1/setup/profile",
                    json={
                        "profile": "ground_station",
                        "ground_role": value,
                        "auto_restart": False,
                    },
                    timeout=2.0,
                )
            except Exception as exc:  # noqa: BLE001
                ctx.logger.debug("settings_role_post_failed", error=str(exc))
        page._bump_pending_reboot(ctx)
        await page._refresh(ctx, force=True)

    await ctx.navigator.push_modal(
        EnumPickerModal(
            title="Role",
            options=options,
            current=str(current) if current is not None else None,
            on_save=_save,
        ),
        ctx=ctx,
    )


__all__ = [
    "_channel_enum",
    "_tx_power_slider",
    "_mcs_enum",
    "_topology_enum",
    "_auto_pair_toggle",
    "_role_enum",
]
