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

# Below this, assume the parse broke rather than that the table shrank. The
# routing tests pin the exact count; this only has to catch "matched nothing".
MIN_NATIVE_ROUTES = 100
MIN_CASES = 40


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


def _python_routes() -> set[tuple[str, str]]:
    """The (METHOD, path) pairs the residual FastAPI app mounts.

    Each router is included under `/api`; a router may add its own prefix, which
    is why several live paths carry `/api/v1/...`. The WHEP router is mounted at
    the root, so its paths are recorded both with and without the `/api` stem.
    """
    routes: set[tuple[str, str]] = set()
    for path in PY_ROUTES_DIR.rglob("*.py"):
        text = path.read_text()
        prefix_match = re.search(r'APIRouter\(\s*prefix="([^"]+)"', text)
        prefix = prefix_match.group(1) if prefix_match else ""
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
        f"parsed only {len(native)} native routes from {ROUTING_RS.name}; "
        "the parse is broken, so this check would pass vacuously"
    )
    assert len(cases) >= MIN_CASES, (
        f"parsed only {len(cases)} conformance cases; the parse is broken"
    )
    assert python, "parsed no residual Python routes; the parse is broken"

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
