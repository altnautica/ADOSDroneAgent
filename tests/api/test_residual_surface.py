"""The residual app mounts nothing outside the front's permanent prefixes.

The native front forwards only these prefixes (``PERMANENT_PYTHON_PREFIXES`` in
``crates/ados-control/src/routing.rs``) and answers every other path itself, so
a route mounted anywhere else could never be reached.
"""

from __future__ import annotations

from collections.abc import Iterable, Iterator
from typing import Any

from ados.api.server import create_app
from tests.api_runtime_utils import build_api_runtime

PERMANENT_PREFIXES = (
    "/api/vision",
    "/api/v1/setup",
    "/api/peripherals",
    "/api/v1/peripherals",
    "/whep",
    "/api/v1/display",
)


def _under_a_permanent_prefix(path: str) -> bool:
    return any(path == p or path.startswith(p + "/") for p in PERMANENT_PREFIXES)


def _mounted_paths(routes: Iterable[Any], prefix: str = "") -> Iterator[str]:
    """Every served path, with the include prefix applied.

    FastAPI keeps an included router as one entry that carries its prefix
    instead of flattening its routes into the app, so walk into it.
    """
    for route in routes:
        path = getattr(route, "path", None)
        if path:
            yield prefix + path
        elif hasattr(route, "include_context"):
            yield from _mounted_paths(
                route.original_router.routes, prefix + route.include_context.prefix
            )


def test_every_residual_route_is_under_a_permanent_prefix() -> None:
    app = create_app(build_api_runtime())
    paths = list(_mounted_paths(app.routes))
    assert paths, "the residual app mounts no routes"
    stray = sorted(p for p in paths if not _under_a_permanent_prefix(p))
    assert stray == [], f"routes the front can never reach: {stray}"
