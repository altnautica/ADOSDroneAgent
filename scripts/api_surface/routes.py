"""The served-route table and literal-to-route resolution."""

from __future__ import annotations

import re
from pathlib import Path

from api_surface.patterns import BASE_PREFIXES, TABLE, TRAILING_JUNK


def served_paths() -> list[tuple[str, list[str]]]:
    """Every served ``(method, path)`` from the committed table, path as segments."""
    if not TABLE.is_file():
        raise SystemExit(f"{TABLE} is missing — run scripts/gen-api-surface.py")
    out: list[tuple[str, list[str]]] = []
    for line in TABLE.read_text(encoding="utf-8").splitlines():
        cells = [c.strip() for c in line.split("|")]
        if len(cells) < 4:
            continue
        path = cells[2].strip("`")
        if path.startswith("/"):
            out.append((cells[1].upper(), path.split("/")))
    if len(out) < 100:
        raise SystemExit(
            f"parsed only {len(out)} paths from {TABLE.name} — the table shape drifted"
        )
    return out


def normalise(raw: str) -> str:
    """Strip a client's interpolation syntax down to a comparable path.

    `${deviceId}`, `{plugin_id}`, `:name` and a bare `*` all stand for one
    segment, which is precisely what a `{param}` template matches.
    """
    path = re.sub(r"\$\{[^}]*\}", "{}", raw)
    path = re.sub(r"\{[^}]*\}", "{}", path)
    path = re.sub(r"<[^>]*>", "{}", path)
    path = re.sub(r"(?<=/):[A-Za-z_][A-Za-z0-9_]*", "{}", path)
    # A placeholder glued to the end of a segment is a query string or a file
    # extension the client appends (`/api/logs/stream${params}`), not another
    # path segment.
    path = re.sub(r"(?<=[^/])\{\}$", "", path)
    return path.rstrip(TRAILING_JUNK) or raw


def matches(actual: list[str], template: list[str]) -> bool:
    """Segment-for-segment match, mirroring `routing.rs::path_matches_template`."""
    if template and template[-1].startswith("{*"):
        head = len(template) - 1
        return len(actual) > head and _segments(actual[:head], template[:head])
    return len(actual) == len(template) and _segments(actual, template)


def _segments(actual: list[str], template: list[str]) -> bool:
    for a, t in zip(actual, template):
        if t.startswith("{") and t.endswith("}"):
            if not a:
                return False
        elif a == "{}":
            # The client interpolates this segment, so its value is not
            # knowable here. Accept it against any single template segment:
            # the check is for paths that no longer exist, and a renamed route
            # still fails on the literal segments around the placeholder.
            continue
        elif a != t:
            return False
    return True


def resolves(path: str, table: list[tuple[str, list[str]]]) -> bool:
    """Whether a client literal names something the agent serves.

    Exact template match, or — only for a listed base-URL constant
    (``BASE_PREFIXES``) — a proper prefix of a served path. A prefix proves the
    family exists, which is enough for a base the client appends to; for any
    other literal it would let an unserved route pass beside a served child.
    """
    segs = path.split("/")
    if any(matches(segs, t) for _, t in table):
        return True
    if path not in BASE_PREFIXES:
        return False
    return any(len(t) > len(segs) and _segments(segs, t[: len(segs)]) for _, t in table)


def exact_methods(path: str, table: list[tuple[str, list[str]]]) -> list[str]:
    """The methods of the most specific routes ``path`` names exactly.

    An interpolated client segment matches any template segment, so
    `/api/plugins/jobs/${id}` also matches `/api/plugins/{plugin_id}/config`.
    The router prefers a literal segment over a parameter, so only the
    templates sharing the most literal segments with the client path count.
    """
    segs = path.split("/")
    scored: list[tuple[int, str]] = []
    for m, t in table:
        if matches(segs, t):
            literal = sum(1 for a, s in zip(segs, t) if a == s and not s.startswith("{"))
            scored.append((literal, m))
    if not scored:
        return []
    best = max(score for score, _ in scored)
    return [m for score, m in scored if score == best]


def method_served(path: str, method: str, table: list[tuple[str, list[str]]]) -> bool:
    """Whether some route matching ``path`` exactly is served for ``method``.

    Only an exact template match is judged; a base-URL prefix names a family,
    not a call, so it carries no method to check.
    """
    exact = exact_methods(path, table)
    return not exact or method in exact


def websocket_only(path: str, table: list[tuple[str, list[str]]]) -> bool:
    """Whether every route ``path`` names exactly answers only an upgrade."""
    exact = exact_methods(path, table)
    return bool(exact) and all(m == "WS" for m in exact)


def owns(path: str, own_routes: list[list[str]]) -> bool:
    """Whether this literal is one of the CLIENT'S OWN server routes.

    Mission Control is a Next.js app: `/api/lan-pair/probe`, `/api/pid-analysis`
    and friends are its own route handlers under `src/app/api/**`, not calls to
    the agent. Matched against each handler's full route, not its first folder:
    the app has an `api/mcp/` folder of its own while the agent also serves
    `/api/mcp/*`, and a first-segment test would exempt every agent MCP call.
    Resolved from the directory tree rather than a hand-kept list, so a new
    server route never has to be added here.
    """
    segs = path.split("/")
    return any(matches(segs, t) for t in own_routes)


def own_server_routes(root: Path) -> list[list[str]]:
    """The client's own route handlers (`app/api/**/route.ts`) as templates."""
    app_dir = root / "app"
    if not (app_dir / "api").is_dir():
        return []
    out: list[list[str]] = []
    for handler in (app_dir / "api").rglob("route.ts*"):
        segs = [""]
        for part in handler.parent.relative_to(app_dir).parts:
            if part.startswith("[...") or part.startswith("[[..."):
                segs.append("{*rest}")
            elif part.startswith("["):
                segs.append("{param}")
            elif not part.startswith("("):
                segs.append(part)
        out.append(segs)
    return out
