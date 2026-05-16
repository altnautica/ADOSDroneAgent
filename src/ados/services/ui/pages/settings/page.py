"""SettingsPage class — scroll, render, and dispatch.

Renders the Settings tab content area as a vertically scrollable
column of 48 px rows. Each row binds a label + current value + handler.
Tapping fires the handler, which typically pushes a modal (enum picker
/ slider / keyboard / confirm dialog) onto the navigator. On modal
save, the handler issues the matching REST call and updates the cached
snapshot the rows draw from.

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
from typing import Any, ClassVar

from PIL import Image, ImageDraw

from ados.services.ui.pages.base import HitZone, PageContext
from ados.services.ui.touch.events import TouchGesture
from ados.services.ui.touch.kinetic import KineticDecay
from ados.services.ui.widgets import (
    BANNER_H,
    ROW_H,
    ConfirmDialog,
    draw_list_row,
    draw_reboot_banner,
)

from ._common import (
    PAGE_H,
    PAGE_W,
    SNAPSHOT_TTL_S,
    _safe_dict,
)
from ._registry import ROW_DEFS
from ._row import Row


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
    refresh_hz: ClassVar[float] = 2.0

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
        self._rows: tuple[Row, ...] = ROW_DEFS

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
        if not force and (now - self._snapshot_at) < SNAPSHOT_TTL_S:
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


__all__ = ["SettingsPage"]
