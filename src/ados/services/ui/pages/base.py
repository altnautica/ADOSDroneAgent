"""Page protocol, shared dataclasses, and PageContext.

Every LCD page implements :class:`Page`. The page navigator paints the
active page into a 480x244 region (the LCD canvas minus the 32 px top
chrome and 44 px bottom tab bar). Pages own their own hit-test zones
and react to gestures from the touch bridge.

Design notes
------------

* Pages are pure async classes — no global state, no class-level
  caches that survive between instances. The navigator constructs each
  page once and reuses it; pages should hold instance state, not
  module state.
* ``hit_zones`` returns a flat list of :class:`HitZone` rectangles in
  page-local coordinates (0..480 x 0..244). The page navigator
  translates the page-local origin to the LCD-global y=32 origin
  before doing the dispatch.
* ``on_touch`` receives the ``HitZone`` whose rectangle contained the
  gesture's ``start_x, start_y`` plus the full :class:`TouchGesture`
  so the page can branch on tap vs swipe vs drag.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import TYPE_CHECKING, ClassVar, Protocol

if TYPE_CHECKING:  # pragma: no cover
    import httpx
    import structlog
    from PIL.Image import Image as PILImage

    from ados.services.ui.renderers.framebuffer import FrameBufferRenderer
    from ados.services.ui.theme import Palette
    from ados.services.ui.touch.events import TouchGesture


@dataclass(frozen=True)
class HitZone:
    """A rectangular touch target on a page or in the chrome.

    Coordinates are in the coordinate system of the surface the zone
    belongs to. For page hit zones that means page-local 0..480 x
    0..244. For chrome zones (tab bar) it means LCD-global 0..480 x
    276..320.

    ``debounce_ms`` lets a page suppress duplicate hits within a tight
    window — useful when a tap on the dashboard tile would otherwise
    fire twice if the operator's stylus skips on the panel.
    """

    id: str
    x: int
    y: int
    w: int
    h: int
    debounce_ms: int = 250

    def contains(self, x: int, y: int) -> bool:
        """Return True if the point ``(x, y)`` lies in this zone."""
        return self.x <= x < self.x + self.w and self.y <= y < self.y + self.h


@dataclass
class PageContext:
    """Render + touch context handed to every Page method.

    ``state`` is a free-form snapshot of agent state — currently the
    same dict the OLED screens consume. ``palette`` is resolved once
    at the top of every render tick by the navigator and passed down
    so pages do not re-read config.yaml. ``http`` is the shared async
    client for any REST calls a page makes; tests can leave this None
    and assert behavior on hit zones / on_touch directly. ``logger``
    is a structlog bound logger so each page emits structured events
    under a consistent namespace.
    """

    state: dict
    palette: Palette
    hostname: str
    http: httpx.AsyncClient | None
    framebuffer: FrameBufferRenderer | None
    navigator: PageNavigator
    logger: structlog.BoundLogger


class Page(Protocol):
    """The contract every page implements.

    ``id`` is the stable string the navigator uses for routing and
    persistence. ``refresh_hz`` lets a page declare its preferred
    redraw cadence; the dashboard runs at 5 Hz, the video page bumps
    to 20 Hz when an appsink frame is available.
    """

    id: ClassVar[str]
    refresh_hz: ClassVar[float]

    async def on_enter(self, ctx: PageContext) -> None:
        """Called once when the page becomes active."""
        ...

    async def on_leave(self, ctx: PageContext) -> None:
        """Called once when the page is replaced by another."""
        ...

    async def render(self, ctx: PageContext) -> PILImage:
        """Return the 480x244 RGB Image for this page."""
        ...

    def hit_zones(self, ctx: PageContext) -> list[HitZone]:
        """Return the active hit zones in page-local coordinates."""
        ...

    async def on_touch(
        self,
        ctx: PageContext,
        zone: HitZone,
        gesture: TouchGesture,
    ) -> None:
        """Handle a gesture whose start point landed in ``zone``."""
        ...


# Forward reference — populated by pages/__init__.py at import time.
class PageNavigator(Protocol):  # pragma: no cover - structural only
    active_page_id: str

    async def go(self, page_id: str) -> None: ...
    async def push_modal(self, page: Page) -> None: ...
    async def pop_modal(self) -> None: ...
    def current_page(self) -> Page: ...
