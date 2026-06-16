"""Reusable value-parity harness for logd-read-migration routes.

Every read-route migration follows the same shape: a route reads logd-first,
falls back to a live source, and the two paths must return the IDENTICAL
response. This harness captures the live-handler output and the logd-derived
output and asserts equality, so each lane's test is three lines.
"""

from __future__ import annotations

from collections.abc import Awaitable, Callable
from typing import Any
from unittest.mock import patch


async def assert_route_parity(
    *,
    call_route: Callable[[], Awaitable[dict[str, Any]]],
    source_target: str,
    logd_signals: Any,
    derive: Callable[[Any], Any] | None = None,
    ignore_fields: set[str] = frozenset(),
) -> None:
    """Assert the logd-derived response == the live-fallback response.

    * ``call_route``    – zero-arg coroutine that invokes the route handler.
    * ``source_target`` – import path of the route's logd source helper to patch
                          (e.g. "ados.api.telemetry_source.latest_hw_signals").
    * ``logd_signals``  – the mock store payload returned when the source is live.
    * ``derive``        – optional; when the route derives via a second helper,
                          its target is patched through to the real impl so the
                          captured logd path runs the real mapping.
    * ``ignore_fields`` – top-level keys that legitimately differ (e.g. live-only
                          ``cpu_count`` from os.cpu_count(), or a wall-clock ts).
    """
    # 1. Live path: source returns None ⇒ route runs its live fallback.
    with patch(source_target, return_value=None):
        live = await call_route()

    # 2. Logd path: source returns the captured signals ⇒ route runs the
    #    store-derived branch over the REAL derive helper.
    with patch(source_target, return_value=logd_signals):
        derived = await call_route()

    live_cmp = {k: v for k, v in live.items() if k not in ignore_fields}
    derived_cmp = {k: v for k, v in derived.items() if k not in ignore_fields}
    assert derived_cmp == live_cmp, (
        f"logd-derived response diverged from live:\n"
        f"  derived={derived_cmp}\n  live={live_cmp}"
    )
