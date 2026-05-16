"""Shared helpers for the Settings page sub-modules.

These bits used to live at the top of the single-file ``settings.py``
module. They are pulled into a private ``_common`` module so the
per-domain handler files can import them without each having to redeclare
the helpers or import sibling files for trivia.
"""

from __future__ import annotations

from typing import Any

from ados.services.ui.pages.base import PageContext

# Settings tab content area inside the navigator (480x244 minus the
# bottom tab bar). Page-level constants stay here so render code in
# ``page.py`` and any future sub-page can reference one truth.
PAGE_W = 480
PAGE_H = 244

# Snapshot refresh window. The page revalidates its cached state from
# the agent every 2 seconds so a peripheral change made elsewhere
# (CLI, GCS) is visible without restarting the LCD service.
SNAPSHOT_TTL_S = 2.0


def _safe_dict(value: Any) -> dict:
    """Return ``value`` if it is already a dict, else an empty dict.

    The TUI snapshots are fed from REST responses that occasionally
    arrive shaped as something other than a dict (None, list, str on
    error). This wrapper keeps the row resolvers tidy.
    """

    return value if isinstance(value, dict) else {}


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


__all__ = [
    "PAGE_W",
    "PAGE_H",
    "SNAPSHOT_TTL_S",
    "_safe_dict",
    "_post_apply",
]
