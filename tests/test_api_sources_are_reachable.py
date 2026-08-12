# SPDX-License-Identifier: GPL-3.0-only
# Copyright (C) 2026 Altnautica — ADOS Drone Agent
"""Every store-read source module must be reachable from a route.

`api/sources/` is the Python layer that reconstructs a route body out of the
logging store. As routes migrate to the native Rust front, the module behind
each one stops being called — and nothing notices, because a module with no
importer breaks no build and fails no test. Three of the five modules here had
already gone that way before this check existed: `gs` and `video` had no
production importer at all, and `mesh` had one that was itself dead.

That last case is why this walks REACHABILITY rather than asking "does anything
import it". `mesh` was imported by `gs`, so a one-hop check would have called it
live while the only thing reaching it was another orphan. The question is
whether a module is reachable from code a request can actually enter, so the
walk starts at the route tree and follows imports from there.

This is the reader-without-producer rule inverted: a producer with no reader.
Both are the same defect — a surface and its supply disagreeing about whether
the other exists — and both are invisible until someone greps.
"""

from __future__ import annotations

import ast
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
SRC = REPO_ROOT / "src"
SOURCES_DIR = SRC / "ados" / "api" / "sources"
ROUTES_DIR = SRC / "ados" / "api" / "routes"

# A broken scan (a wrong glob, a moved package) would make this pass vacuously.
# The floor only has to be above zero: the real assertion is the orphan set
# below, and this package legitimately shrinks as each domain's read route moves
# to the native front. It is down to one module -- `network` -- from five, so
# when that last one migrates, this file and the package go together rather than
# the floor being nudged again.
MIN_SOURCE_MODULES = 1


def _module_name(path: Path) -> str:
    """`src/ados/api/sources/wfb.py` -> `ados.api.sources.wfb`."""
    return ".".join(path.relative_to(SRC).with_suffix("").parts)


def _imports_of(path: Path) -> set[str]:
    """Every dotted module this file imports, absolute form only.

    Parsed rather than grepped so a name inside a string or a comment cannot be
    mistaken for an import -- the thing that makes an orphan look reachable.
    """
    try:
        tree = ast.parse(path.read_text())
    except (OSError, SyntaxError):
        return set()
    out: set[str] = set()
    for node in ast.walk(tree):
        if isinstance(node, ast.Import):
            out.update(alias.name for alias in node.names)
        elif isinstance(node, ast.ImportFrom) and node.module and node.level == 0:
            out.add(node.module)
            out.update(f"{node.module}.{alias.name}" for alias in node.names)
    return out


def _reachable_from_routes() -> set[str]:
    """Every module reachable by following imports out of the route tree."""
    seen: set[str] = set()
    frontier = [p for p in ROUTES_DIR.rglob("*.py")]
    # Seed with the routes themselves so a source imported by another source
    # only counts when that chain terminates in a real route.
    while frontier:
        path = frontier.pop()
        name = _module_name(path)
        if name in seen:
            continue
        seen.add(name)
        for imported in _imports_of(path):
            if not imported.startswith("ados."):
                continue
            candidate = SRC / Path(*imported.split(".")).with_suffix(".py")
            if candidate.is_file() and _module_name(candidate) not in seen:
                frontier.append(candidate)
    return seen


def test_every_api_source_module_is_reachable_from_a_route() -> None:
    modules = sorted(p for p in SOURCES_DIR.glob("*.py") if p.name != "__init__.py")

    assert len(modules) >= MIN_SOURCE_MODULES, (
        f"found only {len(modules)} modules under {SOURCES_DIR.name}/; either the "
        "scan is broken or the package shrank that far -- if the latter, lower "
        "the floor deliberately rather than deleting this check"
    )

    reachable = _reachable_from_routes()
    # Guard the guard: an import walk that resolved nothing would report every
    # module an orphan, which reads as a real finding and is not one.
    assert len(reachable) > 20, (
        f"the import walk reached only {len(reachable)} modules from "
        f"{ROUTES_DIR}; it is broken, so its orphan list means nothing"
    )

    orphans = [_module_name(p) for p in modules if _module_name(p) not in reachable]

    assert not orphans, (
        "these store-read modules are not reachable from any route, so nothing "
        "a request can enter ever calls them:\n"
        + "\n".join(f"  {name}" for name in orphans)
        + "\nA module reached only by another orphan counts as unreachable too. "
        "Delete it with its tests, or wire the route that was meant to call it."
    )


def test_the_reachability_walk_follows_a_chain_not_just_one_hop() -> None:
    """The walk must be transitive, which is what the orphan case needed.

    `mesh` was imported by `gs`, and `gs` was imported by nothing. A one-hop
    check would have called `mesh` live. This asserts the walk reaches a module
    that is only ever imported by another module, rather than one a route names
    directly -- so a future rewrite back to one-hop fails here rather than
    silently narrowing what the check above can see.
    """
    reachable = _reachable_from_routes()
    direct = set()
    for route in ROUTES_DIR.rglob("*.py"):
        direct.update(i for i in _imports_of(route) if i.startswith("ados."))

    indirect = {m for m in reachable if m.startswith("ados.") and m not in direct}
    assert indirect, (
        "the walk reached nothing beyond the routes' own direct imports, so it "
        "is one-hop and cannot see a module kept alive only by another orphan"
    )
