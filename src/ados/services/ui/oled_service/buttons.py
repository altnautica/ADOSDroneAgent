"""Button + overlay helpers for the OLED service.

Hosts the front-panel button consumer, the status/menu dispatchers,
the overlay enter/exit lifecycle (including the on_enter/on_exit
hooks the mesh screens use), and the pairing-window secondary poll
that runs only while the accept overlay is live.
"""

from __future__ import annotations

import asyncio

from .constants import B1, B2, B3, B4, PAIRING_POLL_SECONDS
from .menu_tree import MENU_TREE, _filter_visible, _now
from .screen_registry import OVERLAY_SCREENS


class _ButtonsMixin:
    """Mixin: button bus consumption, overlay lifecycle, pairing poll."""

    def _enter_overlay(
        self,
        screen_id: str,
        initial_state: dict | None = None,
    ) -> None:
        """Switch the display to a mesh overlay screen.

        Fires the previous overlay's `on_exit` before swapping in the
        new module. Nested transitions (e.g. accept_window -> error on
        REST failure) stop their background tasks cleanly instead of
        leaking them.
        """
        from .service import log

        module = OVERLAY_SCREENS.get(screen_id)
        if module is None:
            log.warning("overlay_unknown", screen_id=screen_id)
            return
        prev_id = self._overlay_id
        prev_module = self._overlay_module
        # Call the outgoing overlay's on_exit BEFORE we replace state,
        # so it still sees its own overlay_id/state while cleaning up.
        if prev_module is not None:
            on_exit = getattr(prev_module, "on_exit", None)
            if callable(on_exit):
                try:
                    asyncio.create_task(on_exit(self))
                except Exception as exc:
                    log.debug("overlay_on_exit_failed", screen=prev_id, error=str(exc))
        self._overlay_id = screen_id
        self._overlay_module = module
        # Module-provided initial state takes precedence over caller-
        # supplied overrides so screens can compute from live state.
        if hasattr(module, "initial_state"):
            try:
                base = module.initial_state(self)
            except Exception as exc:
                log.debug("overlay_initial_state_failed", screen=screen_id, error=str(exc))
                base = {}
        else:
            base = {}
        if initial_state:
            base.update(initial_state)
        self._overlay_state = base
        self._mode = "overlay"
        log.info("overlay_entered", screen_id=screen_id, previous=prev_id)
        # Screen-level on_enter hook (e.g. the accept_window overlay
        # opens the pairing window via REST when the operator enters).
        on_enter = getattr(module, "on_enter", None)
        if callable(on_enter):
            try:
                asyncio.create_task(on_enter(self))
            except Exception as exc:
                log.debug("overlay_on_enter_failed", screen=screen_id, error=str(exc))

    def _exit_overlay(self) -> None:
        from .service import log

        prev_id = self._overlay_id
        prev_module = self._overlay_module
        self._overlay_id = None
        self._overlay_module = None
        self._overlay_state = {}
        self._mode = "status"
        log.info("overlay_exited", screen_id=prev_id)
        on_exit = getattr(prev_module, "on_exit", None) if prev_module else None
        if callable(on_exit):
            try:
                asyncio.create_task(on_exit(self))
            except Exception as exc:
                log.debug("overlay_on_exit_failed", screen=prev_id, error=str(exc))

    def _start_pairing_poll(self) -> None:
        if self._pairing_poll_task is not None and not self._pairing_poll_task.done():
            return
        self._pairing_poll_task = asyncio.create_task(
            self._poll_pairing_forever(), name="oled_pairing_poll"
        )

    def _stop_pairing_poll(self) -> None:
        task = self._pairing_poll_task
        if task is not None and not task.done():
            task.cancel()
        self._pairing_poll_task = None

    async def _poll_pairing_forever(self) -> None:
        """Refresh pairing snapshot at 2 Hz while the accept overlay is live."""
        if self._http is None:
            return
        while not self._stop.is_set() and self._overlay_id == "accept_window":
            try:
                r = await self._http.get(f"{self._api_base}/pair/pending", timeout=0.9)
                if r.status_code == 200:
                    data = r.json()
                    if isinstance(data, dict):
                        self._state.setdefault("pairing", {})["window"] = {
                            "open": data.get("open", False),
                            "opened_at_ms": data.get("opened_at_ms"),
                            "closes_at_ms": data.get("closes_at_ms"),
                        }
                        self._state["pairing"]["pending"] = data.get("pending") or []
            except Exception:
                pass
            try:
                await asyncio.wait_for(self._stop.wait(), timeout=PAIRING_POLL_SECONDS)
            except TimeoutError:
                continue

    async def _consume_buttons(self) -> None:
        """Drain the button bus and update UI state."""
        from .service import log

        async for ev in self._bus.subscribe():
            if self._stop.is_set():
                return
            self._last_button_ts = _now()
            # Wake from dim on any press.
            if self._dimmed:
                self._set_contrast(self._brightness_active)
                self._dimmed = False
            # Pixel-invert burn-in protection. On any button press,
            # return the display to natural orientation and reset the
            # invert clock so the user always sees the non-inverted
            # view right after they interact.
            if self._inverted:
                self._set_invert(False)
            self._last_invert_ts = _now()
            if ev.kind != "short":
                # Long-press hooks reach the system-level handler
                # regardless of overlay. One example is factory reset
                # on long B4. Trace for bench visibility.
                if self._mode == "overlay":
                    log.info("oled_long_press_during_overlay", button=ev.button)
                else:
                    log.info("oled_long_press_passthrough", button=ev.button)
                continue
            if self._mode == "overlay":
                await self._handle_overlay_press(ev.button)
            elif self._mode == "unset":
                # Any press moves operator into the Mesh -> Set role menu.
                self._mode = "menu"
                self._menu_stack = []
                self._menu_items = _filter_visible(MENU_TREE, self._state)
                self._menu_sel = 0
                # Auto-enter Mesh submenu if present.
                for i, node in enumerate(self._menu_items):
                    if node.get("label") == "Mesh":
                        self._menu_stack.append((self._menu_items, i))
                        self._menu_items = _filter_visible(
                            node.get("children") or [], self._state
                        )
                        self._menu_sel = 0
                        # Position the cursor on "Set role" explicitly
                        # so the next B3 always drives the role picker
                        # even if a future change adds earlier items.
                        for j, child in enumerate(self._menu_items):
                            if child.get("label") == "Set role":
                                self._menu_sel = j
                                break
                        break
            elif self._mode == "status":
                self._handle_status_press(ev.button)
            else:
                self._handle_menu_press(ev.button)

    async def _handle_overlay_press(self, button: int) -> None:
        from .service import log

        module = self._overlay_module
        if module is None:
            self._exit_overlay()
            return
        actions = getattr(module, "BUTTON_ACTIONS", None) or {}
        handler = actions.get(button)
        if handler is None:
            # Unmapped button in overlay: B4 always exits as a safe default.
            if button == B4:
                self._exit_overlay()
            return
        try:
            await handler(self)
        except Exception as exc:
            log.warning(
                "overlay_action_failed",
                screen_id=self._overlay_id,
                button=button,
                error=str(exc),
            )

    def _handle_status_press(self, button: int) -> None:
        n = max(1, len(self._active_screens))
        if button == B1:
            self._screen_idx = (self._screen_idx - 1) % n
        elif button == B2:
            self._screen_idx = (self._screen_idx + 1) % n
        elif button == B3:
            self._mode = "menu"
            self._menu_stack = []
            self._menu_items = _filter_visible(MENU_TREE, self._state)
            self._menu_sel = 0
        elif button == B4:
            # No-op on status auto-cycle. Stay put.
            pass

    def _handle_menu_press(self, button: int) -> None:
        from .service import log

        if not self._menu_items:
            # Empty after filtering; back out.
            if self._menu_stack:
                parent_items, parent_sel = self._menu_stack.pop()
                self._menu_items = _filter_visible(parent_items, self._state)
                self._menu_sel = min(parent_sel, max(0, len(self._menu_items) - 1))
            else:
                self._mode = "status"
            return
        if button == B1:
            self._menu_sel = (self._menu_sel - 1) % len(self._menu_items)
        elif button == B2:
            self._menu_sel = (self._menu_sel + 1) % len(self._menu_items)
        elif button == B3:
            current = self._menu_items[self._menu_sel]
            screen_id = current.get("screen")
            children = current.get("children") or []
            if current.get("label") == "Back to status":
                self._mode = "status"
                return
            if screen_id:
                # Menu leaf drives an overlay screen.
                self._enter_overlay(screen_id)
                return
            if children:
                self._menu_stack.append((self._menu_items, self._menu_sel))
                self._menu_items = _filter_visible(children, self._state)
                self._menu_sel = 0
            else:
                path = [
                    (items[idx].get("label", "") if idx < len(items) else "")
                    for (items, idx) in self._menu_stack
                ]
                path.append(current.get("label", ""))
                log.info(
                    "menu_action_stub",
                    label=current.get("label"),
                    path=path,
                )
        elif button == B4:
            if self._menu_stack:
                parent_items, parent_sel = self._menu_stack.pop()
                self._menu_items = _filter_visible(parent_items, self._state)
                self._menu_sel = min(parent_sel, max(0, len(self._menu_items) - 1))
            else:
                self._mode = "status"
