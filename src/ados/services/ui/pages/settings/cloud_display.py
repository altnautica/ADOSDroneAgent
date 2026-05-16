"""Cloud-mode and physical-display settings rows.

Two adjacent concerns share this module: the cloud posture selector
(``server.mode``) and the trio of display rows (binding, calibrate,
rotation). Both groups are small enough that a finer split would push
two-handler files; keeping them together preserves a coherent unit.
"""

from __future__ import annotations

from typing import TYPE_CHECKING

from ados.services.ui.pages.base import PageContext
from ados.services.ui.widgets import (
    ConfirmDialog,
    EnumPickerModal,
)

from ._row import Row

if TYPE_CHECKING:
    from .page import SettingsPage


async def _cloud_mode_enum(
    page: "SettingsPage", ctx: PageContext, row: Row,
) -> None:
    options = [
        ("cloud", "Altnautica cloud"),
        ("self_hosted", "Self-hosted"),
        ("local", "Local only (no cloud)"),
    ]
    current = page._value_for(row.id, ctx)

    async def _save(value: str) -> None:
        client = ctx.http
        if client is not None:
            try:
                await client.post(
                    "/api/v1/setup/cloud-choice",
                    json={"mode": value},
                    timeout=2.0,
                )
            except Exception as exc:  # noqa: BLE001
                ctx.logger.debug("settings_cloud_post_failed", error=str(exc))
        await page._refresh(ctx, force=True)

    await ctx.navigator.push_modal(
        EnumPickerModal(
            title="Cloud mode",
            options=options,
            current=str(current) if current is not None else None,
            on_save=_save,
        ),
        ctx=ctx,
    )


async def _display_drilldown(
    page: "SettingsPage", ctx: PageContext, row: Row,
) -> None:
    options: list[tuple[str, str]] = []
    client = ctx.http
    current_id: str | None = None
    if client is not None:
        try:
            r = await client.get(
                "/api/v1/setup/display/options",
                timeout=1.5,
            )
            if r.status_code == 200:
                blob = r.json() if callable(getattr(r, "json", None)) else {}
                if isinstance(blob, dict):
                    cur = blob.get("current") or {}
                    if isinstance(cur, dict):
                        current_id = cur.get("display_id")
                    for entry in blob.get("supported", []):
                        if not isinstance(entry, dict):
                            continue
                        options.append(
                            (
                                str(entry.get("id") or ""),
                                str(entry.get("label") or entry.get("id") or ""),
                            )
                        )
        except Exception as exc:  # noqa: BLE001
            ctx.logger.debug("settings_display_options_failed", error=str(exc))
    if not options:
        options = [("none", "Skip / no display attached")]

    async def _save(value: str) -> None:
        if client is not None:
            try:
                await client.post(
                    "/api/v1/setup/display/install",
                    json={"display_id": value},
                    timeout=5.0,
                )
            except Exception as exc:  # noqa: BLE001
                ctx.logger.debug("settings_display_install_failed", error=str(exc))
        page._bump_pending_reboot(ctx)
        await page._refresh(ctx, force=True)

    await ctx.navigator.push_modal(
        EnumPickerModal(
            title="Display",
            options=options,
            current=current_id,
            on_save=_save,
        ),
        ctx=ctx,
    )


async def _calibrate_action(
    page: "SettingsPage", ctx: PageContext, row: Row,
) -> None:
    async def _on_confirm() -> None:
        client = ctx.http
        if client is not None:
            try:
                await client.post(
                    "/api/v1/setup/display/calibrate/start",
                    timeout=2.0,
                )
            except Exception as exc:  # noqa: BLE001
                ctx.logger.debug("settings_calibrate_failed", error=str(exc))

    await ctx.navigator.push_modal(
        ConfirmDialog(
            "Recalibrate touch",
            (
                "Touch calibration runs the next time the LCD service starts. "
                "Reboot the agent to launch the wizard."
            ),
            confirm_label="Schedule",
            confirm_destructive=False,
            on_confirm=_on_confirm,
        ),
        ctx=ctx,
    )


async def _rotation_enum(
    page: "SettingsPage", ctx: PageContext, row: Row,
) -> None:
    from ados.services.ui.display_conf import (
        ALLOWED_ROTATIONS,
        read_rotation,
        write_rotation,
    )

    options = [(str(v), f"{v}°") for v in ALLOWED_ROTATIONS]
    current = str(read_rotation())

    async def _save(value: str) -> None:
        try:
            write_rotation(int(value))
        except (ValueError, OSError) as exc:
            ctx.logger.warning("settings_rotation_write_failed", error=str(exc))
            return
        page._bump_pending_reboot(ctx)
        await page._refresh(ctx, force=True)

    await ctx.navigator.push_modal(
        EnumPickerModal(
            title="Display rotation",
            options=options,
            current=current,
            on_save=_save,
        ),
        ctx=ctx,
    )


__all__ = [
    "_cloud_mode_enum",
    "_display_drilldown",
    "_calibrate_action",
    "_rotation_enum",
]
