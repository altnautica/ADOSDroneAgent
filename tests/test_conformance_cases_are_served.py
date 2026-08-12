"""Every conformance case must name a route some transport actually serves.

The conformance harness diffs the native front against the residual Python for
each registered case. It compares the two responses to each other and nothing
else, so a case whose route has been deleted from BOTH sides does not fail: both
transports answer 404, the answers match, and the case is reported as passing.
The run then claims coverage of a route that no longer exists.

That is the failure mode a route deletion creates, and it is silent precisely
when the harness is being used to prove a deletion was safe. This check is
static -- it reads the two route tables rather than issuing requests -- so it
runs in CI and cannot introduce a false failure on a rig.

The Rust table is parsed from source. A parse that silently matched nothing
would make this whole check vacuous, so the parse result is itself asserted
against the count the routing tests pin.
"""

from __future__ import annotations

import re
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[1]
ROUTING_RS = REPO_ROOT / "crates" / "ados-control" / "src" / "routing.rs"
ROUTE_CASES = (
    REPO_ROOT / "tools" / "api-conformance" / "api_conformance" / "route_cases.py"
)
PY_ROUTES_DIR = REPO_ROOT / "src" / "ados" / "api" / "routes"

# Floors for "the parse broke". A broken parse returns zero or near-zero, so these
# only have to sit clearly above that -- they are not a route-count contract.
#
# The previous values were set just under the real counts on the theory that a
# tight floor also catches silent shrinkage. It does not, and it made the file
# hostile: with cases at 63 against a floor of 60, this test's own remedy
# ("delete the case with the route") failed on its fourth use, reporting "the
# parse is broken" about a parse that was working perfectly. A guard whose advice
# breaks it is worse than no guard, because the next reader deletes it.
#
# Silent shrinkage is covered where it can be covered exactly: routing.rs pins the
# native route count to an exact number, and the orphan check below is the real
# work of this file. So these are deliberately loose.
MIN_NATIVE_ROUTES = 75
MIN_CASES = 30
MIN_PYTHON_ROUTES = 45


def _native_routes() -> set[tuple[str, str]]:
    """The (METHOD, path) pairs `native_routes()` registers."""
    src = ROUTING_RS.read_text()
    marker = "fn native_routes()"
    if marker not in src:
        # Renamed or restructured. Say so plainly rather than letting the split
        # below raise an IndexError that reads as a bug in this test.
        raise AssertionError(
            f"{ROUTING_RS.name} no longer defines `{marker}`; update this "
            "check's parser to match, or it will stop protecting anything"
        )
    body = src.split(marker, 1)[1].split("\n}", 1)[0]
    return {
        (m.group(1).upper(), m.group(2))
        for m in re.finditer(r'\b(get|post|put|delete)\("([^"]+)"\)', body)
    }


def _router_prefix(text: str) -> str:
    """The `prefix=` a module's own `APIRouter(...)` declares, or ``""``."""
    match = re.search(r'APIRouter\([^)]*prefix="([^"]+)"', text)
    return match.group(1) if match else ""


def _python_routes() -> set[tuple[str, str]]:
    """The (METHOD, path) pairs the residual FastAPI app mounts.

    Every router is included under `/api`. Two things then add to the path, and
    reading a file in isolation sees only one of them:

    * the module's own `APIRouter(prefix=...)`, and
    * the prefix declared by the `__init__.py` of the package it lives in,
      which composes its sub-modules with `include_router`.

    Missing the second is not cosmetic. `routes/setup/__init__.py` declares
    `prefix="/v1/setup"` and includes six sub-modules that each declare a bare
    `APIRouter()`, so ignoring it records nineteen routes at `/api/status`,
    `/api/apply`, `/api/reset` and so on — generic paths that then certify a
    conformance case against a route that does not exist there, while a case
    naming the real `/api/v1/setup/...` path reads as served by nobody.

    Verified against the tree: `routes/setup/` is the only package declaring a
    prefix on its `__init__.py` (`ground_station/` and `video/` declare none and
    their sub-modules carry their own), and no `include_router` call anywhere
    adds a prefix of its own. So one level of package prefix is the whole rule.
    """
    routes: set[tuple[str, str]] = set()
    package_prefix: dict[Path, str] = {}

    for init in PY_ROUTES_DIR.rglob("__init__.py"):
        package_prefix[init.parent] = _router_prefix(init.read_text())

    for path in PY_ROUTES_DIR.rglob("*.py"):
        text = path.read_text()
        # A package's own __init__ must not inherit its prefix twice.
        owner = package_prefix.get(path.parent, "") if path.name != "__init__.py" else ""
        prefix = owner + _router_prefix(text)
        for m in re.finditer(r'@router\.(get|post|put|delete|patch)\("([^"]*)"', text):
            routes.add((m.group(1).upper(), "/api" + prefix + m.group(2)))

    routes |= {(m, p.replace("/api", "", 1)) for m, p in routes if "/whep" in p}
    return routes


def _cases() -> list[tuple[str, str, str]]:
    """The (name, METHOD, path) triples the conformance registry declares."""
    src = ROUTE_CASES.read_text()
    out: list[tuple[str, str, str]] = []
    for block in re.finditer(r"RouteCase\((.*?)\n    \)", src, re.S):
        blk = block.group(1)
        name = re.search(r'name="([^"]+)"', blk)
        method = re.search(r'method="([^"]+)"', blk)
        path = re.search(r'path="([^"]+)"', blk)
        if name and method and path:
            out.append((name.group(1), method.group(1), path.group(1)))
    return out


def _matches(template: str, actual: str) -> bool:
    """Match a request path against a route template with `{param}` segments."""
    tc, ac = template.split("/"), actual.split("/")
    if tc and tc[-1].startswith("{*"):
        return len(ac) >= len(tc) and all(
            t == a or (t.startswith("{") and t.endswith("}"))
            for t, a in zip(tc[:-1], ac[: len(tc) - 1])
        )
    if len(tc) != len(ac):
        return False
    return all(
        t == a or (t.startswith("{") and t.endswith("}")) for t, a in zip(tc, ac)
    )


def _served_by(table: set[tuple[str, str]], method: str, path: str) -> bool:
    return any(m == method and _matches(p, path) for m, p in table)


@pytest.mark.skipif(
    not ROUTING_RS.exists() or not ROUTE_CASES.exists(),
    reason="routing table or conformance registry not present in this checkout",
)
def test_every_conformance_case_names_a_served_route() -> None:
    native = _native_routes()
    python = _python_routes()
    cases = _cases()

    # Guard the guard: a broken parse must fail here, not quietly pass every
    # case because the tables it compares against came back empty.
    assert len(native) >= MIN_NATIVE_ROUTES, (
        f"parsed only {len(native)} native routes from {ROUTING_RS.name}, which is "
        f"below the {MIN_NATIVE_ROUTES} floor. Either the regex stopped matching "
        "the table -- in which case this check would pass vacuously -- or the "
        "native surface genuinely shrank that far, in which case lower the floor "
        "deliberately"
    )
    assert len(cases) >= MIN_CASES, (
        f"parsed only {len(cases)} conformance cases, below the {MIN_CASES} floor. "
        "Either the RouteCase regex stopped matching, or the registry genuinely "
        "shrank that far -- if the latter, lower the floor deliberately"
    )
    # A truthiness check here would pass on a single bogus entry. The Python
    # table currently contributes no matches at all (every case is served
    # natively), so nothing else exercises it.
    assert len(python) >= MIN_PYTHON_ROUTES, (
        f"parsed only {len(python)} residual Python routes, below the "
        f"{MIN_PYTHON_ROUTES} floor. Either the @router regex or the package-prefix "
        "resolution broke, or the residual surface genuinely shrank that far -- the "
        "migration is meant to drive this number to zero, so on the day it does, "
        "this floor and the Python table go together"
    )

    orphans = [
        (name, method, path)
        for name, method, path in cases
        if not _served_by(native, method, path)
        and not _served_by(python, method, path)
    ]

    assert not orphans, (
        "conformance cases name routes that neither transport serves, so they "
        "compare two 404s and report a pass while proving nothing:\n"
        + "\n".join(f"  {m} {p}  ({n})" for n, m, p in orphans)
        + "\nDelete the case with the route, or restore the route."
    )
