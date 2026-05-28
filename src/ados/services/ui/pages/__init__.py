"""Page registry, navigator, and modal stack for the LCD UI.

The navigator owns the active page id, persists it to
``/run/ados/lcd-state.json`` so a service restart returns the operator
to the page they were on, and dispatches gestures to the active page
or topmost modal.

Page registration is import-driven: a page module declares an
instance and imports register it through :func:`register_page`. The
navigator exposes ``go(page_id)`` to switch routes; ``push_modal`` /
``pop_modal`` for transient overlays (settings sub-views, confirm
dialogs).

The persistence write is atomic (tmpfile + rename) so a power cut
mid-write cannot leave a half-flushed JSON file.
"""

from __future__ import annotations

import asyncio
import json
import os
import tempfile
from pathlib import Path

from ados.core.logging import get_logger
from ados.core.paths import LCD_STATE_PATH

from .base import HitZone, Page, PageContext

log = get_logger("ui.pages")


class PageNavigator:
    """Active-page state machine + modal stack.

    A single instance is created by the OLED service and passed to
    every page through :class:`PageContext`. Pages reach back into
    the navigator to switch routes (``await ctx.navigator.go(...)``)
    or push a modal sub-view.
    """

    DEFAULT_PAGE_ID = "dashboard"

    def __init__(
        self,
        registry: dict[str, Page] | None = None,
        *,
        state_path: Path | None = None,
    ) -> None:
        self._registry: dict[str, Page] = dict(registry or {})
        self.active_page_id: str = self.DEFAULT_PAGE_ID
        self.modal_stack: list[Page] = []
        self._tap_feedback: dict[str, int] = {}
        self._state_path = state_path or LCD_STATE_PATH
        self._lock = asyncio.Lock()
        # Restore previous active id from disk if present.
        prev = self._read_persisted_id()
        if prev and prev in self._registry:
            self.active_page_id = prev
        elif prev:
            # Persisted id no longer registered (renamed page). Leave
            # the default in place but log so a regression is visible.
            log.warning("page_navigator_persisted_id_unknown", page_id=prev)
        # Persist the resolved initial state so /run/ados/lcd-state.json
        # exists on first boot. Without this, the file is only written
        # when go() runs after operator navigation, and the cloud
        # heartbeat consumer reads an empty state until the operator
        # touches the LCD. Only write when the file is absent so a valid
        # persisted id is never clobbered by a default that resolved
        # before pages were registered (pages may register after init).
        if not self._state_path.exists():
            self._persist_active_id(self.active_page_id)

    # ── registration ────────────────────────────────────────────

    def register(self, page: Page) -> None:
        """Make ``page`` discoverable by id."""
        self._registry[page.id] = page

    def has(self, page_id: str) -> bool:
        return page_id in self._registry

    def page(self, page_id: str) -> Page | None:
        return self._registry.get(page_id)

    def known_page_ids(self) -> tuple[str, ...]:
        return tuple(self._registry.keys())

    # ── navigation ──────────────────────────────────────────────

    async def go(self, page_id: str, ctx: PageContext | None = None) -> bool:
        """Switch to ``page_id``. Returns True if the route changed."""
        async with self._lock:
            if page_id == self.active_page_id and not self.modal_stack:
                return False
            if page_id not in self._registry:
                log.warning("page_navigator_go_unknown", page_id=page_id)
                return False
            current = self.current_page()
            if ctx is not None:
                try:
                    await current.on_leave(ctx)
                except Exception as exc:  # noqa: BLE001
                    log.warning(
                        "page_on_leave_failed",
                        page_id=current.id,
                        error=str(exc),
                    )
            # Drop any open modal when the route changes outright.
            self.modal_stack = []
            self.active_page_id = page_id
            self._persist_active_id(page_id)
            new_page = self._registry[page_id]
            if ctx is not None:
                try:
                    await new_page.on_enter(ctx)
                except Exception as exc:  # noqa: BLE001
                    log.warning(
                        "page_on_enter_failed",
                        page_id=page_id,
                        error=str(exc),
                    )
            return True

    async def push_modal(
        self, page: Page, ctx: PageContext | None = None,
    ) -> None:
        """Push a transient overlay above the current page."""
        async with self._lock:
            self.modal_stack.append(page)
            if ctx is not None:
                try:
                    await page.on_enter(ctx)
                except Exception as exc:  # noqa: BLE001
                    log.warning(
                        "modal_on_enter_failed",
                        page_id=page.id,
                        error=str(exc),
                    )

    async def pop_modal(self, ctx: PageContext | None = None) -> Page | None:
        async with self._lock:
            if not self.modal_stack:
                return None
            page = self.modal_stack.pop()
            if ctx is not None:
                try:
                    await page.on_leave(ctx)
                except Exception as exc:  # noqa: BLE001
                    log.warning(
                        "modal_on_leave_failed",
                        page_id=page.id,
                        error=str(exc),
                    )
            return page

    def current_page(self) -> Page:
        """Return the topmost modal or the active page."""
        if self.modal_stack:
            return self.modal_stack[-1]
        page = self._registry.get(self.active_page_id)
        if page is None:
            # Should not happen — go() validates before mutating — but
            # we never want to crash the render loop. Fall through to
            # any registered page so the chrome still paints.
            if not self._registry:
                raise RuntimeError("no pages registered")
            return next(iter(self._registry.values()))
        return page

    # ── tap feedback bookkeeping ────────────────────────────────

    def record_tap(self, zone_id: str, now_ms: int) -> None:
        """Surface a tap-flash for the next render tick."""
        self._tap_feedback[zone_id] = now_ms

    def tap_feedback(self) -> dict[str, int]:
        return dict(self._tap_feedback)

    # ── persistence ─────────────────────────────────────────────

    def _persist_active_id(self, page_id: str) -> None:
        blob = {
            "active_page_id": page_id,
            "modal_stack": [p.id for p in self.modal_stack],
        }
        try:
            self._state_path.parent.mkdir(parents=True, exist_ok=True)
            fd, tmp = tempfile.mkstemp(
                prefix=self._state_path.name + ".",
                suffix=".tmp",
                dir=str(self._state_path.parent),
            )
            with os.fdopen(fd, "w") as fh:
                json.dump(blob, fh, separators=(",", ":"))
                fh.flush()
                os.fsync(fh.fileno())
            os.replace(tmp, self._state_path)
        except OSError as exc:  # noqa: BLE001
            log.debug(
                "page_navigator_persist_failed",
                error=str(exc),
                state_path=str(self._state_path),
            )

    def _read_persisted_id(self) -> str | None:
        try:
            blob = json.loads(self._state_path.read_text())
        except (OSError, json.JSONDecodeError):
            return None
        if not isinstance(blob, dict):
            return None
        page_id = blob.get("active_page_id")
        return page_id if isinstance(page_id, str) else None


__all__ = ["HitZone", "Page", "PageContext", "PageNavigator"]
