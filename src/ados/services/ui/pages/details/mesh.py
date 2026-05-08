"""Mesh detail page.

Drilldown opened from the dashboard's MESH tile. Shows the active
role with a switch-role button, a scrollable peer list (up to 6
visible rows), and a footer line carrying gateway short-id +
partition status.

Switching role pushes a minimal in-page picker overlay rather than
a full enum-modal widget (the full widget lands later). Picking a
new role posts to ``POST /api/v1/setup/profile`` with
``{profile: "ground_station", ground_role: <role>, auto_restart: false}``.

Touch behaviour:

* Tap the back chevron to pop the modal.
* Tap "Switch role" toggles the role-picker overlay over the bottom
  half of the page; tap a row to commit, tap "Switch role" again or
  the back chevron to dismiss.
* Drag inside the peer list scrolls the list (kinetic decay handled
  upstream by the touch bridge for swipe gestures; this page reads
  ``gesture.end_y - gesture.start_y`` on the final drag for a
  one-shot scroll offset).
"""

from __future__ import annotations

import asyncio
from typing import Any, ClassVar

from PIL import Image, ImageDraw

from ados.services.ui.dashboards.components import primitives as p
from ados.services.ui.touch.events import TouchGesture

from ..base import HitZone, PageContext
from ._common import HEADER_H, draw_header_band

PAGE_W = 480
PAGE_H = 244

ROLE_BADGE_DOT_R = 5
ROLE_ROW_Y = HEADER_H + 4
SWITCH_BTN_X = PAGE_W - 100
SWITCH_BTN_Y = ROLE_ROW_Y
SWITCH_BTN_W = 90
SWITCH_BTN_H = 24
PEER_LIST_Y = HEADER_H + 36
PEER_LIST_H = PAGE_H - PEER_LIST_Y - 28
PEER_ROW_H = 24
PEER_ROWS_VISIBLE = 6
FOOTER_Y = PAGE_H - 28


def _safe_dict(value: Any) -> dict:
    return value if isinstance(value, dict) else {}


def _role_color(role: str, palette) -> tuple[int, int, int]:  # type: ignore[no-untyped-def]
    role = (role or "").lower()
    if role == "direct":
        return palette.text_secondary
    if role == "relay":
        return palette.accent_primary
    if role == "receiver":
        return palette.status_success
    return palette.text_tertiary


class MeshDetailPage:
    """Detail view for the MESH dashboard tile."""

    id: ClassVar[str] = "details.mesh"
    refresh_hz: ClassVar[float] = 2.0

    def __init__(self) -> None:
        self._picker_open: bool = False
        self._scroll_offset: int = 0
        self._switch_in_flight: asyncio.Task | None = None
        self._tick: int = 0  # for the "checking" rotating dot

    async def on_enter(self, ctx: PageContext) -> None:
        ctx.logger.info("details_mesh_enter")

    async def on_leave(self, ctx: PageContext) -> None:
        ctx.logger.info("details_mesh_leave")
        if self._switch_in_flight is not None and not self._switch_in_flight.done():
            self._switch_in_flight.cancel()
            try:
                await self._switch_in_flight
            except (asyncio.CancelledError, Exception):
                pass
        self._switch_in_flight = None

    async def render(self, ctx: PageContext) -> Image.Image:
        self._tick += 1
        palette = ctx.palette
        img = Image.new("RGB", (PAGE_W, PAGE_H), palette.bg_primary)
        draw_header_band(img, palette, "Mesh")
        d = ImageDraw.Draw(img)

        role_block = _safe_dict(ctx.state.get("role"))
        mesh_block = _safe_dict(ctx.state.get("mesh"))
        role = (role_block.get("current") or "direct").lower()
        mesh_capable = bool(role_block.get("mesh_capable"))

        # Role badge row.
        dot_color = _role_color(role, palette)
        d.ellipse(
            (
                12,
                ROLE_ROW_Y + 6,
                12 + ROLE_BADGE_DOT_R * 2,
                ROLE_ROW_Y + 6 + ROLE_BADGE_DOT_R * 2,
            ),
            fill=dot_color,
        )
        role_font = p.font("sans_bold", 14)
        d.text(
            (28, ROLE_ROW_Y + 4),
            role.upper(),
            fill=palette.text_primary,
            font=role_font,
        )
        # Switch role button.
        d.rectangle(
            (
                SWITCH_BTN_X,
                SWITCH_BTN_Y,
                SWITCH_BTN_X + SWITCH_BTN_W - 1,
                SWITCH_BTN_Y + SWITCH_BTN_H - 1,
            ),
            fill=palette.bg_secondary,
            outline=palette.border_strong,
            width=1,
        )
        switch_label = "Switch role"
        switch_font = p.font("sans_bold", 11)
        sw, sh = p.text_size(img, switch_label, switch_font)
        d.text(
            (
                SWITCH_BTN_X + (SWITCH_BTN_W - sw) // 2,
                SWITCH_BTN_Y + (SWITCH_BTN_H - sh) // 2 - 1,
            ),
            switch_label,
            fill=palette.text_primary,
            font=switch_font,
        )

        # Body.
        if role == "direct" or not mesh_capable and role == "direct":
            self._render_direct_body(img, d, palette)
        elif not mesh_block.get("up"):
            self._render_mesh_down_body(img, d, palette)
        else:
            self._render_peer_list(img, d, palette, mesh_block)

        # Footer: gateway + partition status.
        gw = mesh_block.get("selected_gateway") or "--"
        partition = bool(mesh_block.get("partition"))
        footer_font = p.font("mono_regular", 11)
        footer_text = f"gw {gw}"
        d.text(
            (12, FOOTER_Y + 6),
            footer_text,
            fill=palette.text_secondary,
            font=footer_font,
        )
        if partition:
            label = "PARTITIONED"
            label_font = p.font("sans_bold", 11)
            lw, _ = p.text_size(img, label, label_font)
            d.rectangle(
                (PAGE_W - lw - 16, FOOTER_Y + 4, PAGE_W - 8, FOOTER_Y + 22),
                fill=palette.status_warning,
            )
            d.text(
                (PAGE_W - lw - 12, FOOTER_Y + 6),
                label,
                fill=palette.bg_primary,
                font=label_font,
            )

        if self._picker_open:
            self._render_picker(img, d, palette, role)
        return img

    def _render_direct_body(
        self,
        img: Image.Image,
        d: ImageDraw.ImageDraw,
        palette,  # type: ignore[no-untyped-def]
    ) -> None:
        msg = "Not a mesh node"
        font = p.font("sans_bold", 14)
        mw, mh = p.text_size(img, msg, font)
        d.text(
            ((PAGE_W - mw) // 2, PEER_LIST_Y + (PEER_LIST_H - mh) // 2 - 6),
            msg,
            fill=palette.text_secondary,
            font=font,
        )
        sub = "this node is in direct role"
        sub_font = p.font("sans_regular", 11)
        sw, _ = p.text_size(img, sub, sub_font)
        d.text(
            ((PAGE_W - sw) // 2, PEER_LIST_Y + (PEER_LIST_H - mh) // 2 + 14),
            sub,
            fill=palette.text_tertiary,
            font=sub_font,
        )

    def _render_mesh_down_body(
        self,
        img: Image.Image,
        d: ImageDraw.ImageDraw,
        palette,  # type: ignore[no-untyped-def]
    ) -> None:
        msg = "Mesh down -- checking"
        font = p.font("sans_bold", 14)
        mw, mh = p.text_size(img, msg, font)
        d.text(
            ((PAGE_W - mw) // 2, PEER_LIST_Y + (PEER_LIST_H - mh) // 2 - 4),
            msg,
            fill=palette.status_warning,
            font=font,
        )
        # Rotating dot — placement orbits a small circle to make
        # progress visible without a real animation framework.
        import math

        cx = PAGE_W // 2
        cy = PEER_LIST_Y + (PEER_LIST_H - mh) // 2 + 22
        angle = (self._tick % 8) * (math.pi / 4)
        radius = 6
        dx = int(round(math.cos(angle) * radius))
        dy = int(round(math.sin(angle) * radius))
        d.ellipse(
            (cx + dx - 3, cy + dy - 3, cx + dx + 3, cy + dy + 3),
            fill=palette.accent_primary,
        )

    def _render_peer_list(
        self,
        img: Image.Image,
        d: ImageDraw.ImageDraw,
        palette,  # type: ignore[no-untyped-def]
        mesh_block: dict,
    ) -> None:
        peers = mesh_block.get("peers")
        if not isinstance(peers, list):
            peers = []
        # Apply scroll offset (rows above the visible window).
        offset = max(0, min(len(peers) - PEER_ROWS_VISIBLE, self._scroll_offset))
        visible = peers[offset : offset + PEER_ROWS_VISIBLE]
        row_font = p.font("mono_regular", 10)
        badge_font = p.font("sans_bold", 10)
        seen_font = p.font("mono_regular", 10)
        for i, peer in enumerate(visible):
            if not isinstance(peer, dict):
                continue
            row_y = PEER_LIST_Y + i * PEER_ROW_H
            d.line(
                (8, row_y + PEER_ROW_H - 1, PAGE_W - 8, row_y + PEER_ROW_H - 1),
                fill=palette.border_default,
            )
            dev = str(peer.get("device_id") or peer.get("id") or "--")
            short = dev[-12:]
            d.text(
                (12, row_y + 6),
                short,
                fill=palette.text_primary,
                font=row_font,
            )
            role = (peer.get("role") or "").lower()
            badge_color = _role_color(role, palette)
            d.text(
                (220, row_y + 6),
                role.upper() if role else "--",
                fill=badge_color,
                font=badge_font,
            )
            seen = peer.get("last_seen_seconds_ago")
            seen_text = f"{int(seen)}s" if isinstance(seen, (int, float)) else "--"
            sw, _ = p.text_size(img, seen_text, seen_font)
            d.text(
                (PAGE_W - sw - 12, row_y + 6),
                seen_text,
                fill=palette.text_tertiary,
                font=seen_font,
            )
        if not visible:
            empty_font = p.font("sans_regular", 11)
            msg = "no peers visible"
            mw, _ = p.text_size(img, msg, empty_font)
            d.text(
                ((PAGE_W - mw) // 2, PEER_LIST_Y + 30),
                msg,
                fill=palette.text_tertiary,
                font=empty_font,
            )

    def _render_picker(
        self,
        img: Image.Image,
        d: ImageDraw.ImageDraw,
        palette,  # type: ignore[no-untyped-def]
        current: str,
    ) -> None:
        # Overlay covers the bottom half of the body.
        ovr_y = PEER_LIST_Y - 4
        d.rectangle(
            (4, ovr_y, PAGE_W - 4, PAGE_H - 4),
            fill=palette.bg_secondary,
            outline=palette.border_strong,
            width=1,
        )
        choices = ("direct", "relay", "receiver")
        row_h = (PAGE_H - 4 - ovr_y) // len(choices)
        row_font = p.font("sans_bold", 14)
        for i, choice in enumerate(choices):
            ry = ovr_y + i * row_h
            is_current = choice == current
            if is_current:
                d.rectangle(
                    (4, ry, PAGE_W - 4, ry + row_h - 1),
                    fill=palette.bg_tertiary,
                )
            label = choice.upper()
            lw, lh = p.text_size(img, label, row_font)
            d.text(
                ((PAGE_W - lw) // 2, ry + (row_h - lh) // 2 - 1),
                label,
                fill=palette.accent_primary if is_current else palette.text_primary,
                font=row_font,
            )

    def hit_zones(self, ctx: PageContext) -> list[HitZone]:
        zones: list[HitZone] = [
            HitZone(id="details.back", x=8, y=8, w=40, h=32),
            HitZone(
                id="mesh.switch_role",
                x=SWITCH_BTN_X,
                y=SWITCH_BTN_Y,
                w=SWITCH_BTN_W,
                h=SWITCH_BTN_H,
            ),
        ]
        if self._picker_open:
            ovr_y = PEER_LIST_Y - 4
            row_h = (PAGE_H - 4 - ovr_y) // 3
            for i, choice in enumerate(("direct", "relay", "receiver")):
                zones.append(
                    HitZone(
                        id=f"mesh.role.{choice}",
                        x=4,
                        y=ovr_y + i * row_h,
                        w=PAGE_W - 8,
                        h=row_h,
                    )
                )
        else:
            # Peer rows (no-op tap targets for now; future revs can
            # focus a peer or open a sub-detail).
            for i in range(PEER_ROWS_VISIBLE):
                zones.append(
                    HitZone(
                        id=f"mesh.peer.{i}",
                        x=8,
                        y=PEER_LIST_Y + i * PEER_ROW_H,
                        w=PAGE_W - 16,
                        h=PEER_ROW_H,
                    )
                )
        return zones

    async def _post_role(self, ctx: PageContext, role: str) -> None:
        client = ctx.http
        if client is None:
            return
        try:
            await client.post(
                "/api/v1/setup/profile",
                json={
                    "profile": "ground_station",
                    "ground_role": role,
                    "auto_restart": False,
                },
                timeout=2.0,
            )
            ctx.logger.info("details_mesh_role_post", role=role)
        except Exception as exc:  # noqa: BLE001
            ctx.logger.debug("details_mesh_role_post_failed", error=str(exc))

    async def on_touch(
        self,
        ctx: PageContext,
        zone: HitZone,
        gesture: TouchGesture,
    ) -> None:
        if zone.id == "details.back" and gesture.kind == "tap":
            if self._picker_open:
                self._picker_open = False
                return
            await ctx.navigator.pop_modal(ctx=ctx)
            return
        if zone.id == "mesh.switch_role" and gesture.kind == "tap":
            self._picker_open = not self._picker_open
            return
        if zone.id.startswith("mesh.role.") and gesture.kind == "tap":
            choice = zone.id.split(".", 2)[2]
            if choice in ("direct", "relay", "receiver"):
                self._picker_open = False
                if self._switch_in_flight is not None and not self._switch_in_flight.done():
                    self._switch_in_flight.cancel()
                self._switch_in_flight = asyncio.create_task(
                    self._post_role(ctx, choice)
                )
            return
        if zone.id.startswith("mesh.peer.") and gesture.kind == "tap":
            ctx.logger.info("details_mesh_peer_focus", peer_zone=zone.id)
            return
        # Drag inside the peer list area = scroll.
        if (
            gesture.kind in ("drag", "swipe")
            and PEER_LIST_Y <= gesture.start_y - 32 < PEER_LIST_Y + PEER_LIST_H
        ):
            delta_y = gesture.start_y - gesture.end_y
            self._scroll_offset = max(
                0, self._scroll_offset + delta_y // PEER_ROW_H
            )
