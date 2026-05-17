"""Display-side helpers for the OLED service.

Hosts the screen-paint dispatcher, OLED hardware knobs (contrast,
invert), the SIGHUP-driven UI config reload, the role badge overlay
for mesh-capable nodes, the host-name probe, and the async LCD page
renderer that paints chrome + active page onto the SPI LCD
framebuffer.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any

from PIL import Image

from ados.core.config import load_config
from ados.services.ui.chrome import bottom_tab_bar, top_status_bar
from ados.services.ui.theme import current_palette

from .constants import WIDTH
from .screen_registry import (
    DEFAULT_SCREEN_ENABLED,
    DEFAULT_SCREEN_ORDER,
    SCREEN_RENDERERS,
    screen_menu,
)
from .screen_registry import unset_boot_screen as screen_mesh_unset_boot


class _DisplayMixin:
    """Mixin: screen paint, OLED knobs, config reload, role badge."""

    def _paint_active_screen(self, draw: Any, width: int, height: int) -> None:
        """Dispatch the active mode/screen onto a PIL ImageDraw canvas.

        Centralizes the screen-selection logic so the OLED render loop
        and the framebuffer render loop call the same code.
        """
        if self._mode == "overlay" and self._overlay_module is not None:
            overlay_state = {
                **self._state,
                "_overlay_state": self._overlay_state,
            }
            self._overlay_module.render(draw, width, height, overlay_state)
        elif self._mode == "unset":
            screen_mesh_unset_boot.render(draw, width, height, self._state)
        elif self._mode == "status" and self._active_screens:
            _, module = self._active_screens[self._screen_idx]
            module.render(draw, width, height, self._state)
            self._render_role_badge(draw)
        elif self._mode == "menu":
            screen_menu.render(
                draw,
                width,
                height,
                {
                    "items": [n.get("label", "") for n in self._menu_items],
                    "selected": self._menu_sel,
                    "depth": len(self._menu_stack),
                },
            )

    def _read_hostname(self) -> str:
        try:
            return Path("/etc/hostname").read_text().strip() or "groundnode"
        except OSError:
            return "groundnode"

    async def _render_lcd_page(self) -> None:
        """Paint chrome + active page onto the framebuffer.

        Async because the page protocol's :meth:`render` is async.
        Called from the async ``_render_forever`` loop directly.
        """
        from .service import log

        if self._fb_renderer is None or self._page_navigator is None:
            return
        palette = current_palette()
        # Refresh context state every tick — palette flip and state
        # poll updates need to reach the page without restart.
        if self._page_context is not None:
            self._page_context.palette = palette
            self._page_context.state = self._state
        canvas = Image.new("RGB", (480, 320), palette.bg_primary)
        # Top status bar.
        top_status_bar.draw(
            canvas,
            0,
            0,
            480,
            palette=palette,
            hostname=self._page_context.hostname if self._page_context else "groundnode",
            state=self._state,
        )
        # Active page paints into the 480x244 region just below the
        # 32 px chrome. Modal stack is rendered on top by the
        # current_page() resolution.
        page = self._page_navigator.current_page()
        page_img: Image.Image | None = None
        try:
            page_img = await page.render(self._page_context)  # type: ignore[arg-type]
        except Exception as exc:  # noqa: BLE001
            log.warning("page_render_failed", page_id=page.id, error=str(exc))
        if page_img is not None:
            canvas.paste(page_img, (0, 32))
        # Bottom tab bar with any active feedback flashes.
        bottom_tab_bar.draw(
            canvas,
            0,
            320 - 44,
            480,
            palette=palette,
            active=self._page_navigator.active_page_id,
            tapped_at_ms=self._page_navigator.tap_feedback(),
        )
        try:
            self._fb_renderer.present(canvas)
        except Exception as exc:  # noqa: BLE001
            log.warning("framebuffer_present_failed", error=str(exc))

    def _set_contrast(self, value: int) -> None:
        from .service import log

        if self._device is None:
            return
        try:
            self._device.contrast(value)
        except Exception as exc:
            log.debug("contrast_failed", error=str(exc))

    def _set_invert(self, on: bool) -> None:
        from .service import log

        if self._device is None:
            return
        try:
            # luma.oled devices expose `.invert(bool)` on most drivers.
            invert_fn = getattr(self._device, "invert", None)
            if callable(invert_fn):
                invert_fn(on)
                self._inverted = on
        except Exception as exc:
            log.debug("invert_failed", error=str(exc))

    def _reload_ui_config(self) -> None:
        """Rebuild active screen list, brightness, and cycle period from config.

        Called once at construction and again whenever SIGHUP fires. Tolerates
        a missing `ground_station.ui` block by falling back to defaults. Never
        raises: if config load itself blows up we keep the prior state.
        """
        from .service import log

        try:
            cfg = load_config()
            ui = getattr(getattr(cfg, "ground_station", None), "ui", None)
            screens_cfg = getattr(ui, "screens", {}) if ui is not None else {}
            oled_cfg = getattr(ui, "oled", {}) if ui is not None else {}
        except Exception as exc:
            log.warning("ui_config_reload_failed", error=str(exc))
            return

        order = screens_cfg.get("order") if isinstance(screens_cfg, dict) else None
        enabled = screens_cfg.get("enabled") if isinstance(screens_cfg, dict) else None
        if not isinstance(order, list) or not order:
            order = list(DEFAULT_SCREEN_ORDER)
        if not isinstance(enabled, list) or not enabled:
            enabled = list(DEFAULT_SCREEN_ENABLED)

        enabled_set = set(enabled)
        active: list[tuple[str, Any]] = []
        for sid in order:
            if not isinstance(sid, str):
                continue
            if sid not in enabled_set:
                continue
            renderer = SCREEN_RENDERERS.get(sid)
            if renderer is None:
                continue
            active.append((sid, renderer))
        if not active:
            # Empty active set is unusable. Fall back to defaults so the
            # operator always has something on screen.
            active = [(sid, SCREEN_RENDERERS[sid]) for sid in DEFAULT_SCREEN_ORDER]

        self._active_screens = active
        # Clamp screen_idx in case the list shrank under us.
        if self._screen_idx >= len(self._active_screens):
            self._screen_idx = 0

        if isinstance(oled_cfg, dict):
            cycle = oled_cfg.get("screen_cycle_seconds")
            if isinstance(cycle, (int, float)) and cycle > 0:
                self._cycle_seconds = float(cycle)
            auto_dim = oled_cfg.get("auto_dim_enabled")
            if isinstance(auto_dim, bool):
                self._auto_dim_enabled = auto_dim
            brightness = oled_cfg.get("brightness")
            if isinstance(brightness, int) and 0 <= brightness <= 255:
                self._brightness_active = brightness
                if not self._dimmed:
                    self._set_contrast(brightness)

        log.info(
            "oled_ui_config_reloaded",
            screens=[sid for sid, _ in self._active_screens],
            cycle_s=self._cycle_seconds,
            auto_dim=self._auto_dim_enabled,
            brightness=self._brightness_active,
        )

    def request_reload(self) -> None:
        """SIGHUP entry point. Set a flag the render loop will pick up."""
        self._reload_requested = True

    def _render_role_badge(self, draw: Any) -> None:
        """Draw a compact role indicator at the top-right of the status cycle.

        Kept tight (3-char role tag, optional 3-char mesh_id suffix) so
        it fits in the ~30 px strip beyond where status screens like
        `link.py` draw channel text at x=88. The badge renders at or
        past x=94 to avoid overlap.
        """
        role_block = self._state.get("role") or {}
        role = role_block.get("current")
        mesh_capable = role_block.get("mesh_capable", False)
        if not mesh_capable:
            return
        mesh_block = self._state.get("mesh") or {}
        if role == "receiver":
            mesh_id = str(mesh_block.get("mesh_id") or "")[:3]
            label = f"Rx{mesh_id}" if mesh_id else "Rx"
        elif role == "relay":
            label = "Rly"
        elif role == "direct":
            label = "Dir"
        else:
            label = "?"
        label = label[:5]
        # Right-anchor at WIDTH with a 6px per glyph approximation and
        # a minimum left bound of 94 to stay clear of channel text.
        approx_px = len(label) * 6
        x = max(94, WIDTH - approx_px - 2)
        draw.text((x, 0), label, fill="white")
