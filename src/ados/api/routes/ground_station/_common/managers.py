"""Lazy-import singletons for the live ground-station service managers.

Each helper takes no args and returns the relevant manager. Lazy
imports keep the route module loadable without the service-layer
dependencies wired up — useful during testing and during early-boot
ordering.

These are the canonical monkeypatch targets used by route tests
(``monkeypatch.setattr(gs, "_pair_manager", ...)``). The package-level
re-export in ``ground_station/__init__.py`` ensures that pattern
keeps working after the package split.
"""

from __future__ import annotations

from typing import Any


def _pair_manager() -> Any:
    """Return the process-wide PairManager. Lazy import so route module loads without it."""
    from ados.services.ground_station.pair_manager import get_pair_manager

    return get_pair_manager()


__all__ = [
    "_pair_manager",
]
