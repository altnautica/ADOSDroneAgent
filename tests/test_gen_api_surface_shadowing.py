"""The API-surface table must not lose a route to method-blind shadowing.

``docs/api-surface.md`` is the file ``scripts/check-api-surface.py`` resolves
every client path literal against, so a row missing from it is a client call
with nothing to check. The generator drops the residual copy of any route the
native front also serves — and it used to decide that by PATH alone.

``routing::is_native`` is method-scoped, so one URL can be split between the
two producers: ``GET /api/video/config`` is answered by the native front while
``POST /api/video/config`` is forwarded to the residual FastAPI app. Path-blind
shadowing erased that POST from the table entirely. Nothing about the output
showed it: the table looked complete, asserted the front owned a route it
actually forwards, and the write had no row of its own.

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


def test_the_live_table_carries_the_split_video_config_route() -> None:
    """End-to-end on the committed artefact, not on the generator's internals.

    `POST /api/video/config` is served by `ados.api.routes.video.encoder_config`
    and is the concrete route the path-blind shadow deleted. If the table loses
    it again, `check-api-surface.py` resolves the GCS's write call against the
    native GET row and reports success for a route it never checked.
    """
    table = (
        Path(__file__).resolve().parent.parent / "docs" / "api-surface.md"
    ).read_text(encoding="utf-8")

    assert "| POST | `/api/video/config` |" in table
    assert "| GET | `/api/video/config` |" in table
