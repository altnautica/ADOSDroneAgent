"""The API-surface table must not lose a route to method-blind shadowing.

``docs/api-surface.md`` is the file ``scripts/check-api-surface.py`` resolves
every client path literal against, so a row missing from it is a client call
with nothing to check. The generator drops the residual copy of any route the
native front also serves — and it used to decide that by PATH alone.

``routing::is_native`` is method-scoped, so one URL can be split between the
two producers: a native ``GET`` beside a residual ``POST`` on the same path.
Path-blind shadowing erased that POST from the table entirely. Nothing about
the output showed it: the table looked complete, asserted the front owned a
route it actually forwards, and the write had no row of its own.

The generator itself shells `cargo`, so these exercise the shadow decision
directly — the part that was wrong — rather than the whole run.
"""

from __future__ import annotations

import importlib.util
from pathlib import Path

_SCRIPT = Path(__file__).resolve().parent.parent / "scripts" / "gen-api-surface.py"
_spec = importlib.util.spec_from_file_location("gen_api_surface", _SCRIPT)
assert _spec is not None and _spec.loader is not None
gen = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(gen)


def test_a_split_path_keeps_both_owners() -> None:
    """The regression: a native GET must not swallow a residual POST."""
    native = [("GET", "/api/video/config")]
    residual = [("POST", "/api/video/config")]

    assert gen.shadow_native(native, residual) == [("POST", "/api/video/config")]


def test_the_same_route_on_both_sides_is_shadowed() -> None:
    """The behaviour the shadow exists for: one route, one owner, no duplicate."""
    native = [("PUT", "/api/v1/ground-station/wfb")]
    residual = [("PUT", "/api/v1/ground-station/wfb")]

    assert gen.shadow_native(native, residual) == []


def test_an_unrelated_residual_route_survives() -> None:
    native = [("GET", "/api/status")]
    residual = [("POST", "/api/plugins/install"), ("GET", "/api/logs")]

    assert gen.shadow_native(native, residual) == residual


def test_a_websocket_route_is_recorded_as_ws_not_get() -> None:
    """A GET against an upgrade-only route 404s, so the table must not say GET."""
    from fastapi import APIRouter, WebSocket

    router = APIRouter()

    @router.websocket("/plugins/jobs/{job_id}")
    async def _stream(websocket: WebSocket, job_id: str) -> None:  # pragma: no cover
        await websocket.close()

    @router.get("/plugins")
    async def _list() -> list[str]:  # pragma: no cover
        return []

    out: list[tuple[str, str]] = []
    gen._walk(router, "/api", out)

    assert ("WS", "/api/plugins/jobs/{job_id}") in out
    assert ("GET", "/api/plugins/jobs/{job_id}") not in out
    assert ("GET", "/api/plugins") in out
