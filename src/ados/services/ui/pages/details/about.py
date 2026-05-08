"""About page — read-only system identity drilldown.

Shipped from the settings list as the bottom-most row. The body shows
agent version, board name + manufacturer, device id, MAC addresses
for the active network interfaces, build date (release-time stamp),
repo URL, and license. All values are read-only; the operator can
back out via the chevron.

Data sources:

* ``GET /api/v1/setup/status`` — top-level snapshot. Carries
  ``version``, ``device_id``, ``device_name``, board info via the
  setup state machine.
* ``/sys/class/net/<iface>/address`` — MAC addresses for any
  interface that exposes a sysfs file (eth0, wlan0, wlan1).
* ``/etc/ados/build.txt`` — optional release-time stamp written by
  the install script.

Each fetch is best-effort; missing values render as ``--``.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any, ClassVar

from PIL import Image, ImageDraw

from ados.services.ui.dashboards.components import primitives as p
from ados.services.ui.touch.events import TouchGesture

from ..base import HitZone, PageContext
from ._common import HEADER_H, draw_header_band

PAGE_W = 480
PAGE_H = 244


def _safe_get(d: Any, key: str, default: str = "--") -> str:
    if isinstance(d, dict):
        v = d.get(key)
        if v is not None and v != "":
            return str(v)
    return default


def _read_mac(iface: str) -> str:
    try:
        return Path(f"/sys/class/net/{iface}/address").read_text().strip() or "--"
    except OSError:
        return "--"


def _read_build_stamp() -> str:
    try:
        return Path("/etc/ados/build.txt").read_text().strip() or "--"
    except OSError:
        return "--"


def _read_board_name() -> str:
    try:
        from ados.hal.detect import detect_board

        board = detect_board()
        return getattr(board, "name", "") or "--"
    except Exception:
        return "--"


class AboutPage:
    """Read-only About modal pushed from the settings list."""

    id: ClassVar[str] = "details.about"
    refresh_hz: ClassVar[float] = 1.0

    def __init__(self) -> None:
        self._status: dict[str, Any] = {}

    async def on_enter(self, ctx: PageContext) -> None:
        ctx.logger.info("details_about_enter")
        await self._refresh(ctx)

    async def on_leave(self, ctx: PageContext) -> None:
        ctx.logger.info("details_about_leave")

    async def _refresh(self, ctx: PageContext) -> None:
        client = ctx.http
        if client is None:
            return
        try:
            r = await client.get("/api/v1/setup/status", timeout=1.5)
            if r.status_code == 200:
                blob = r.json() if callable(getattr(r, "json", None)) else {}
                if isinstance(blob, dict):
                    self._status = blob
        except Exception as exc:  # noqa: BLE001
            ctx.logger.debug("details_about_status_fetch_failed", error=str(exc))

    async def render(self, ctx: PageContext) -> Image.Image:
        palette = ctx.palette
        img = Image.new("RGB", (PAGE_W, PAGE_H), palette.bg_primary)
        draw_header_band(img, palette, "About")
        d = ImageDraw.Draw(img)

        version = _safe_get(self._status, "version")
        device_id = _safe_get(self._status, "device_id")
        device_name = _safe_get(self._status, "device_name")
        board = _read_board_name()
        eth_mac = _read_mac("eth0")
        wlan_mac = _read_mac("wlan0")
        build = _read_build_stamp()

        rows = [
            ("Agent", version),
            ("Board", board),
            ("Device ID", device_id),
            ("Device", device_name),
            ("eth0", eth_mac),
            ("wlan0", wlan_mac),
            ("Build", build),
            ("License", "GPLv3"),
            ("Repo", "github.com/altnautica/ADOSDroneAgent"),
        ]

        label_font = p.font("sans_bold", 11)
        value_font = p.font("mono_regular", 12)
        row_h = 22
        cy = HEADER_H + 6
        for label, value in rows:
            d.text(
                (16, cy),
                label.upper(),
                fill=palette.text_tertiary,
                font=label_font,
            )
            d.text(
                (140, cy - 1),
                value,
                fill=palette.text_primary,
                font=value_font,
            )
            cy += row_h
        return img

    def hit_zones(self, ctx: PageContext) -> list[HitZone]:
        return [HitZone(id="details.back", x=8, y=8, w=40, h=32)]

    async def on_touch(
        self,
        ctx: PageContext,
        zone: HitZone,
        gesture: TouchGesture,
    ) -> None:
        if zone.id == "details.back" and gesture.kind == "tap":
            await ctx.navigator.pop_modal(ctx=ctx)
