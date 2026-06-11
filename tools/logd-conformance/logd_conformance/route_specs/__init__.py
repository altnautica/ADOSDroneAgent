"""Per-domain conformance RouteSpecs.

Each module exposes ``routes() -> list[RouteSpec]`` for one domain. The
registry below aggregates them. Adding a domain is a one-line append to
``_REGISTRY`` — the only shared edit, serialized in the main thread — while
each domain's spec lives in its own file, so lanes never collide.
"""

from __future__ import annotations

from collections.abc import Callable

from ..routes import RouteSpec
from . import baseline, link, video  # one import line per domain (serialized)

# The ordered registry. A domain provider is a zero-arg callable returning its
# RouteSpecs. Order here is report order.
_REGISTRY: list[Callable[[], list[RouteSpec]]] = [
    baseline.routes,  # logs + hw-summary + hw-snapshot + service-events
    link.routes,  # link-metrics
    video.routes,  # video-metrics
    # <one line per new domain — the only serialized edit>
]


def all_route_specs() -> list[RouteSpec]:
    """Every registered domain's RouteSpecs, in registry order."""
    out: list[RouteSpec] = []
    for provider in _REGISTRY:
        out.extend(provider())
    return out
