"""Settings page — scrollable list of editor rows.

Renders the Settings tab content area (480x244 minus the bottom tab
bar) as a vertically scrollable column of 48 px rows. Each row binds
a label + current value + handler. Tapping fires the handler, which
typically pushes a modal (enum picker / slider / keyboard / confirm
dialog) onto the navigator. On modal save, the handler issues the
matching REST call and updates the cached snapshot the rows draw
from.

The list scrolls via the touch move bus (live drag tracking) plus a
kinetic decay seeded from the gesture's release velocity. Rubber-band
overshoot of 16 px lets a vigorous pull bounce off either end without
locking the offset to a hard floor / ceiling.

A reboot banner sits above the list whenever the operator has made
changes that require a service restart. Tapping the banner pushes a
confirm dialog → ``POST /api/v1/setup/reboot``.
"""

from __future__ import annotations

import asyncio
import time
from collections.abc import Awaitable, Callable
from dataclasses import dataclass
from typing import Any, ClassVar

from PIL import Image, ImageDraw

from ados.services.ui.pages.base import HitZone, PageContext
from ados.services.ui.touch.events import TouchGesture
from ados.services.ui.touch.kinetic import KineticDecay
from ados.services.ui.widgets import (
    BANNER_H,
    ROW_H,
    ConfirmDialog,
    EnumPickerModal,
    KeyboardModal,
    SliderModal,
    draw_list_row,
    draw_reboot_banner,
)

PAGE_W = 480
PAGE_H = 244

# Snapshot refresh window. The page revalidates its cached state from
# the agent every 2 seconds so a peripheral change made elsewhere
# (CLI, GCS) is visible without restarting the LCD service.
_SNAPSHOT_TTL_S = 2.0


@dataclass(frozen=True)
class Row:
    """One settings row.

    ``id`` is a stable key the page uses to look up zones and dispatch
    handlers. ``label`` is the operator-facing copy. ``variant`` chooses
    the row primitive (default / toggle / action). ``handler`` is the
    coroutine fired on tap.
    """

    id: str
    label: str
    variant: str
    handler: Callable[
        [SettingsPage, PageContext, Row], Awaitable[None]
    ]


class SettingsPage:
    """Scrollable settings list.

    The page implements the :class:`Page` protocol from
    :mod:`ados.services.ui.pages.base`. It holds:

    * ``_y_offset`` — current scroll offset in pixels.
    * ``_snapshot_*`` dicts — cached REST responses keyed by source
      endpoint. Each refresh fills these without clobbering the
      previous values until the new payload lands.
    * ``_kinetic`` — :class:`KineticDecay` driving inertial scroll.
    * ``_move_task`` — asyncio task subscribed to the move bus while
      this page is active. Cancelled in :meth:`on_leave`.
    """

    id: ClassVar[str] = "settings"
    refresh_hz: ClassVar[float] = 5.0

    def __init__(self) -> None:
        self._y_offset: int = 0
        self._kinetic = KineticDecay()
        self._move_task: asyncio.Task | None = None
        self._move_active: bool = False
        self._last_render_ms: int = 0
        # Snapshots keyed by source endpoint.
        self._wfb: dict[str, Any] = {}
        self._gs_status: dict[str, Any] = {}
        self._setup_status: dict[str, Any] = {}
        self._network: dict[str, Any] = {}
        self._snapshot_at: float = 0.0
        # Local copy of the row registry. Held on the instance so we
        # can refresh row labels / values from the snapshot without
        # rebuilding the registry on every render.
        self._rows: tuple[Row, ...] = _ROW_DEFS

    # ── lifecycle ──────────────────────────────────────────────

    async def on_enter(self, ctx: PageContext) -> None:
        ctx.logger.info("settings_enter")
        self._snapshot_at = 0.0
        await self._refresh(ctx)
        self._maybe_subscribe_moves(ctx)

    async def on_leave(self, ctx: PageContext) -> None:
        ctx.logger.info("settings_leave")
        self._move_active = False
        if self._move_task is not None and not self._move_task.done():
            self._move_task.cancel()
            try:
                await self._move_task
            except (asyncio.CancelledError, Exception):
                pass
        self._move_task = None
        self._kinetic.stop()

    def _maybe_subscribe_moves(self, ctx: PageContext) -> None:
        if ctx.touch_move_bus is None:
            return
        if self._move_task is not None and not self._move_task.done():
            return
        self._move_active = True
        self._move_task = asyncio.create_task(self._consume_moves(ctx))

    async def _consume_moves(self, ctx: PageContext) -> None:
        bus = ctx.touch_move_bus
        if bus is None:
            return
        last_y: int | None = None
        try:
            async for move in bus.subscribe():
                if not self._move_active:
                    break
                if last_y is None:
                    last_y = move.y_lcd
                    continue
                dy = last_y - move.y_lcd
                last_y = move.y_lcd
                if dy:
                    # Stop any decay while the operator is dragging.
                    self._kinetic.stop()
                    self._scroll_by(dy)
        except asyncio.CancelledError:
            return
        except Exception as exc:  # noqa: BLE001
            ctx.logger.debug("settings_move_loop_failed", error=str(exc))

    # ── snapshot refresh ───────────────────────────────────────

    async def _refresh(self, ctx: PageContext, *, force: bool = False) -> None:
        now = time.monotonic()
        if not force and (now - self._snapshot_at) < _SNAPSHOT_TTL_S:
            return
        client = ctx.http
        self._snapshot_at = now
        if client is None:
            # No HTTP client: fall back to anything the state dict
            # already carries; the row resolvers handle missing data.
            return
        await asyncio.gather(
            self._fetch_wfb(ctx, client),
            self._fetch_gs_status(ctx, client),
            self._fetch_setup_status(ctx, client),
            return_exceptions=True,
        )

    async def _fetch_wfb(self, ctx: PageContext, client) -> None:  # type: ignore[no-untyped-def]
        try:
            r = await client.get("/api/wfb", timeout=1.5)
            if r.status_code == 200:
                blob = r.json() if callable(getattr(r, "json", None)) else {}
                if isinstance(blob, dict):
                    self._wfb = blob
        except Exception as exc:  # noqa: BLE001
            ctx.logger.debug("settings_wfb_fetch_failed", error=str(exc))

    async def _fetch_gs_status(self, ctx: PageContext, client) -> None:  # type: ignore[no-untyped-def]
        try:
            r = await client.get(
                "/api/v1/ground-station/status",
                timeout=1.5,
            )
            if r.status_code == 200:
                blob = r.json() if callable(getattr(r, "json", None)) else {}
                if isinstance(blob, dict):
                    self._gs_status = blob
        except Exception as exc:  # noqa: BLE001
            ctx.logger.debug("settings_gs_status_fetch_failed", error=str(exc))

    async def _fetch_setup_status(self, ctx: PageContext, client) -> None:  # type: ignore[no-untyped-def]
        try:
            r = await client.get("/api/v1/setup/status", timeout=1.5)
            if r.status_code == 200:
                blob = r.json() if callable(getattr(r, "json", None)) else {}
                if isinstance(blob, dict):
                    self._setup_status = blob
                    net = blob.get("network")
                    if isinstance(net, dict):
                        self._network = net
        except Exception as exc:  # noqa: BLE001
            ctx.logger.debug("settings_setup_status_fetch_failed", error=str(exc))

    # ── value resolvers (current state shown in row's right column) ──

    def _value_for(self, row_id: str, ctx: PageContext) -> Any:
        st = ctx.state
        wfb = self._wfb or _safe_dict(st.get("wfb"))
        net = self._network or _safe_dict(st.get("network"))
        gs = self._gs_status
        setup = self._setup_status

        if row_id == "network.hotspot":
            # Setup-status shape uses a flat hotspot_ssid; agent state
            # uses a nested {hotspot: {ssid: ...}}. Try both.
            ssid = net.get("hotspot_ssid") or _safe_dict(
                net.get("hotspot")
            ).get("ssid") or "ADOS-AP"
            return ssid
        if row_id == "network.hotspot.on":
            if "hotspot_enabled" in net:
                return bool(net.get("hotspot_enabled"))
            hotspot = _safe_dict(net.get("hotspot"))
            return bool(hotspot.get("enabled"))
        if row_id == "network.wifi_client":
            wc = _safe_dict(net.get("wifi_client"))
            return wc.get("ssid") or "Not configured"
        if row_id == "wfb.channel":
            return wfb.get("channel")
        if row_id == "wfb.tx_power_dbm":
            v = wfb.get("tx_power_dbm")
            return f"{int(v)} dBm" if isinstance(v, (int, float)) else "--"
        if row_id == "wfb.mcs_index":
            v = wfb.get("mcs_index")
            return f"MCS {int(v)}" if isinstance(v, (int, float)) else "--"
        if row_id == "wfb.topology":
            return wfb.get("topology") or _safe_dict(st.get("radio")).get("topology") or "host_vbus"
        if row_id == "wfb.auto_pair":
            return bool(wfb.get("auto_pair_enabled"))
        if row_id == "ground.role":
            role = _safe_dict(st.get("role")).get("current") or _safe_dict(
                gs.get("role")
            ).get("current")
            return role or "direct"
        if row_id == "server.mode":
            choice = _safe_dict(setup.get("cloud_choice"))
            return choice.get("mode") or "cloud"
        if row_id == "display.binding":
            net_status = _safe_dict(setup.get("network"))
            return net_status.get("api_port") and (setup.get("device_name") or "Display")
        if row_id == "display.rotation":
            from ados.services.ui.display_conf import read_rotation

            return f"{read_rotation()}°"
        if row_id == "ui.theme":
            return _safe_dict(st.get("ui")).get("theme") or "dark"
        if row_id == "logging.level":
            return _safe_dict(st.get("logging")).get("level") or "info"
        return None

    # ── render ─────────────────────────────────────────────────

    async def render(self, ctx: PageContext) -> Image.Image:
        await self._refresh(ctx)
        # Advance kinetic decay before we paint so the operator sees
        # smooth deceleration even during a render-only tick.
        now_ms = int(time.monotonic() * 1000)
        if self._last_render_ms == 0:
            dt = 1.0 / max(self.refresh_hz, 1.0)
        else:
            dt = (now_ms - self._last_render_ms) / 1000.0
        self._last_render_ms = now_ms
        if self._kinetic.active:
            offset_delta = self._kinetic.tick(dt)
            self._scroll_by(int(offset_delta))
        else:
            # Snap back any rubber-band overshoot at rest.
            max_off = self._max_offset()
            if self._y_offset < 0:
                self._y_offset = 0
            elif self._y_offset > max_off:
                self._y_offset = max_off

        palette = ctx.palette
        img = Image.new("RGB", (PAGE_W, PAGE_H), palette.bg_primary)
        d = ImageDraw.Draw(img)

        # Reboot banner sits at the very top of the page when there
        # are pending changes.
        list_top = 0
        pending = int(ctx.state.get("pending_reboot_count", 0) or 0)
        if pending > 0:
            draw_reboot_banner(
                img,
                0,
                0,
                PAGE_W,
                palette=palette,
                count=pending,
            )
            list_top = BANNER_H

        # Visible band for the row list.
        for i, row in enumerate(self._rows):
            row_y = list_top + i * ROW_H - self._y_offset
            if row_y + ROW_H <= list_top:
                continue
            if row_y >= PAGE_H:
                break
            # Clip the row by drawing into a temporary band crop only
            # if it would overflow. Otherwise paint at row_y directly.
            value = self._value_for(row.id, ctx)
            value_str: str | None = None
            state: Any = None
            if row.variant == "toggle":
                state = bool(value)
            elif row.variant == "action":
                state = None  # action rows don't carry state in-list
            else:
                if value is None:
                    value_str = ""
                elif isinstance(value, bool):
                    value_str = "On" if value else "Off"
                else:
                    value_str = str(value)
            draw_list_row(
                img,
                0,
                row_y,
                PAGE_W,
                palette=palette,
                label=row.label,
                value=value_str,
                variant=row.variant,
                state=state,
            )
        # Top fade where rows clip into the banner area.
        d.line(
            (0, list_top, PAGE_W - 1, list_top),
            fill=palette.border_default,
        )
        return img

    # ── geometry helpers ───────────────────────────────────────

    def _max_offset(self) -> int:
        total = ROW_H * len(self._rows)
        list_h = PAGE_H - (BANNER_H if self._has_pending_state() else 0)
        return max(0, total - list_h)

    def _has_pending_state(self) -> bool:
        # The page can't see ctx here; the value is conservative when
        # we don't know — assume no banner so we don't shrink the
        # scroll envelope on operators without pending changes.
        return False

    def _scroll_by(self, dy: int) -> None:
        max_off = self._max_offset()
        new_off = self._y_offset + dy
        new_off = max(-16, min(max_off + 16, new_off))
        self._y_offset = new_off

    # ── hit zones + dispatch ───────────────────────────────────

    def hit_zones(self, ctx: PageContext) -> list[HitZone]:
        zones: list[HitZone] = []
        list_top = 0
        pending = int(ctx.state.get("pending_reboot_count", 0) or 0)
        if pending > 0:
            zones.append(
                HitZone(
                    id="banner.reboot",
                    x=0,
                    y=0,
                    w=PAGE_W,
                    h=BANNER_H,
                )
            )
            list_top = BANNER_H
        for i, row in enumerate(self._rows):
            row_y = list_top + i * ROW_H - self._y_offset
            if row_y + ROW_H <= list_top or row_y >= PAGE_H:
                continue
            zones.append(
                HitZone(
                    id=f"row:{row.id}",
                    x=0,
                    y=max(list_top, row_y),
                    w=PAGE_W,
                    h=min(PAGE_H, row_y + ROW_H) - max(list_top, row_y),
                )
            )
        return zones

    async def on_touch(
        self,
        ctx: PageContext,
        zone: HitZone,
        gesture: TouchGesture,
    ) -> None:
        if zone.id == "banner.reboot" and gesture.kind == "tap":
            await self._show_reboot_dialog(ctx)
            return
        if not zone.id.startswith("row:"):
            return
        if gesture.kind == "drag":
            # Seed kinetic decay from the release velocity. Direction
            # is up/down; positive velocity scrolls the list upward,
            # which means y_offset increases.
            v = gesture.velocity_px_per_s
            if gesture.direction == "down":
                v = -v
            elif gesture.direction != "up":
                # Horizontal drag inside a row is treated as no-op so a
                # finger slip doesn't hijack the scroll.
                return
            self._kinetic.start(v)
            return
        if gesture.kind == "long_press":
            row_id = zone.id.removeprefix("row:")
            # First/last visible row long-press → jump to top/bottom.
            visible = self._visible_row_indexes()
            if not visible:
                return
            target_idx = self._index_for(row_id)
            if target_idx == visible[0]:
                self._y_offset = 0
            elif target_idx == visible[-1]:
                self._y_offset = self._max_offset()
            return
        if gesture.kind != "tap":
            return
        row_id = zone.id.removeprefix("row:")
        row = self._row_for(row_id)
        if row is None:
            return
        try:
            await row.handler(self, ctx, row)
        except Exception as exc:  # noqa: BLE001
            ctx.logger.warning(
                "settings_row_handler_failed",
                row=row.id,
                error=str(exc),
            )

    def _visible_row_indexes(self) -> list[int]:
        list_top = 0
        out: list[int] = []
        for i in range(len(self._rows)):
            row_y = list_top + i * ROW_H - self._y_offset
            if row_y + ROW_H <= list_top or row_y >= PAGE_H:
                continue
            out.append(i)
        return out

    def _index_for(self, row_id: str) -> int:
        for i, r in enumerate(self._rows):
            if r.id == row_id:
                return i
        return -1

    def _row_for(self, row_id: str) -> Row | None:
        for r in self._rows:
            if r.id == row_id:
                return r
        return None

    # ── reboot bookkeeping ─────────────────────────────────────

    def _bump_pending_reboot(self, ctx: PageContext) -> None:
        n = int(ctx.state.get("pending_reboot_count", 0) or 0) + 1
        ctx.state["pending_reboot_count"] = n
        ctx.logger.info("settings_pending_reboot_bumped", count=n)

    async def _show_reboot_dialog(self, ctx: PageContext) -> None:
        async def _on_confirm() -> None:
            await self._commit_reboot(ctx)

        modal = ConfirmDialog(
            "Reboot now",
            "Reboot the agent to apply the pending changes?",
            confirm_label="Reboot",
            confirm_destructive=False,
            on_confirm=_on_confirm,
        )
        await ctx.navigator.push_modal(modal, ctx=ctx)

    async def _commit_reboot(self, ctx: PageContext) -> None:
        client = ctx.http
        if client is None:
            ctx.logger.warning("settings_reboot_no_http")
            return
        try:
            r = await client.post("/api/v1/setup/reboot", timeout=2.0)
            if 200 <= r.status_code < 300:
                ctx.state["pending_reboot_count"] = 0
                ctx.logger.info("settings_reboot_dispatched")
            else:
                ctx.logger.warning(
                    "settings_reboot_rejected",
                    status=r.status_code,
                )
        except Exception as exc:  # noqa: BLE001
            ctx.logger.debug("settings_reboot_failed", error=str(exc))


# ── helpers ────────────────────────────────────────────────────


def _safe_dict(value: Any) -> dict:
    return value if isinstance(value, dict) else {}


# ── handlers ───────────────────────────────────────────────────
# Each handler takes the page, ctx, and row. They push a modal and on
# save commit via the matching REST endpoint. After a commit they
# refresh the snapshot so the row redraws with the new value.


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


async def _channel_enum(
    page: SettingsPage, ctx: PageContext, row: Row,
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
    page: SettingsPage, ctx: PageContext, row: Row,
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
    page: SettingsPage, ctx: PageContext, row: Row,
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
    page: SettingsPage, ctx: PageContext, row: Row,
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
    page: SettingsPage, ctx: PageContext, row: Row,
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
    page: SettingsPage, ctx: PageContext, row: Row,
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


async def _cloud_mode_enum(
    page: SettingsPage, ctx: PageContext, row: Row,
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
    page: SettingsPage, ctx: PageContext, row: Row,
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
    page: SettingsPage, ctx: PageContext, row: Row,
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
    page: SettingsPage, ctx: PageContext, row: Row,
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


async def _theme_enum(
    page: SettingsPage, ctx: PageContext, row: Row,
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
    page: SettingsPage, ctx: PageContext, row: Row,
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
    page: SettingsPage, ctx: PageContext, row: Row,
) -> None:
    await page._show_reboot_dialog(ctx)


async def _factory_reset_action(
    page: SettingsPage, ctx: PageContext, row: Row,
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
    page: SettingsPage, ctx: PageContext, row: Row,
) -> None:
    from ados.services.ui.pages.details.about import AboutDetailPage

    await ctx.navigator.push_modal(AboutDetailPage(), ctx=ctx)


# ── REST helpers ───────────────────────────────────────────────


async def _post_apply(ctx: PageContext, body: dict) -> dict | None:
    """POST a partial body to ``/api/v1/setup/apply`` and return the result.

    Network errors and non-200 responses are swallowed and logged at
    debug level; the caller can re-poll the snapshot to see what
    actually persisted. Returns the parsed JSON body on success.
    """
    client = ctx.http
    if client is None:
        return None
    try:
        r = await client.post(
            "/api/v1/setup/apply",
            json=body,
            timeout=2.0,
        )
        if r.status_code == 200:
            return r.json() if callable(getattr(r, "json", None)) else None
        ctx.logger.debug(
            "settings_apply_non_200",
            status=r.status_code,
            body=body,
        )
    except Exception as exc:  # noqa: BLE001
        ctx.logger.debug(
            "settings_apply_failed",
            error=str(exc),
            body=body,
        )
    return None


# ── row registry ───────────────────────────────────────────────

_ROW_DEFS: tuple[Row, ...] = (
    Row("network.hotspot", "Wi-Fi hotspot", "default", _wifi_hotspot_drilldown),
    Row("network.hotspot.on", "Hotspot enabled", "toggle", _hotspot_toggle),
    Row("network.wifi_client", "Wi-Fi client", "default", _wifi_client_drilldown),
    Row("wfb.channel", "Channel", "default", _channel_enum),
    Row("wfb.tx_power_dbm", "TX power", "default", _tx_power_slider),
    Row("wfb.mcs_index", "MCS index", "default", _mcs_enum),
    Row("wfb.topology", "Topology", "default", _topology_enum),
    Row("wfb.auto_pair", "Auto-pair", "toggle", _auto_pair_toggle),
    Row("ground.role", "Role", "default", _role_enum),
    Row("server.mode", "Cloud mode", "default", _cloud_mode_enum),
    Row("display.binding", "Display", "default", _display_drilldown),
    Row("display.calibrate", "Calibrate touch", "action", _calibrate_action),
    Row("display.rotation", "Display rotation", "default", _rotation_enum),
    Row("ui.theme", "Theme", "default", _theme_enum),
    Row("logging.level", "Log level", "default", _log_level_enum),
    Row("system.reboot", "Reboot now", "action", _reboot_action),
    Row("system.factory_reset", "Factory reset", "action", _factory_reset_action),
    Row("about", "About", "default", _about_drilldown),
)
