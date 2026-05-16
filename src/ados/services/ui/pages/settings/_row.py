"""Row dataclass shared across the settings sub-modules.

Defining :class:`Row` outside the per-domain handler files keeps the
handler signatures (which type-hint ``SettingsPage`` and ``Row``) free
of circular-import gymnastics.
"""

from __future__ import annotations

from collections.abc import Awaitable, Callable
from dataclasses import dataclass
from typing import TYPE_CHECKING, Any

from ados.services.ui.pages.base import PageContext

if TYPE_CHECKING:  # avoid runtime cycle
    from .page import SettingsPage


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
        [SettingsPage, PageContext, Row], Awaitable[Any]
    ]


__all__ = ["Row"]
