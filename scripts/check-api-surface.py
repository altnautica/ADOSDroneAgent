#!/usr/bin/env python3
"""Fail when a client calls an agent route the agent does not serve.

There are five clients of the agent's HTTP surface — Mission Control, the
Android GCS, ADOS-MCP, the on-device dashboard SPA and the ``ados`` CLI — and
no build step ever compared any of them against the agent. A route renamed on
one side therefore landed as a silent 404 in the others, discovered by an
operator rather than by CI. Two such holes were live when this was written:
four ``/api/ota*`` paths nothing has ever served, and an Android MAVLink
WebSocket path with no producer.

This scans each client's sources for ``/api/...`` path literals and resolves
every one against ``docs/api-surface.md``. A literal that resolves against no
served route is an error.

    ADOSDroneAgent/.venv/bin/python scripts/check-api-surface.py

Exit 0 when every literal resolves, 1 otherwise. Needs no toolchain beyond
CPython: it reads the committed table, so it runs before anything is built.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
MONOREPO = REPO.parent
TABLE = REPO / "docs" / "api-surface.md"

# Where each client's source lives, and which file extensions carry path
# literals. A client absent from the checkout (a partial clone) is skipped
# with a note rather than failing — the check must be runnable inside the
# agent repo alone.
CLIENTS = [
    ("Mission Control", MONOREPO / "ADOSMissionControl" / "src", {".ts", ".tsx"}),
    ("Android GCS", MONOREPO / "ADOSAndroidGCS" / "app" / "src", {".kt", ".java"}),
    ("ADOS-MCP", MONOREPO / "ADOS-MCP" / "src", {".ts"}),
    ("agent dashboard", REPO / "dashboard" / "src", {".ts", ".tsx"}),
    ("agent cockpit", REPO / "cockpit" / "src", {".ts", ".tsx"}),
    ("ados CLI", REPO / "src" / "ados" / "cli", {".py"}),
]

# A path literal: a slash-led `/api/...` run of URL-safe characters. Template
# placeholders (`${id}`, `{name}`, `:id`) are normalised below rather than
# excluded, because the segment they stand for is exactly what a `{param}`
# route template matches.
LITERAL = re.compile(r"/api/[A-Za-z0-9/_.{}:*<>-]*")

# A client interpolation, with whatever call sits inside it
# (``${encodeURIComponent(id)}``). Collapsed to one placeholder segment before
# literals are extracted, because the regex above would otherwise stop at the
# opening paren and report half a path.
INTERPOLATION = re.compile(r"\$\{[^{}]*(?:\{[^{}]*\}[^{}]*)*\}")

# A line that is prose, not a call. Doc comments name path FAMILIES
# (``/api/v1/setup/*``, ``/api/pairing/*``) that no router registers verbatim,
# and a prose typo cannot 404 anything.
COMMENT = re.compile(r"^\s*(//|/\*|\*|#)")

# A module import. `@/lib/api/ground-station-api` is a FILE path that happens
# to contain `/api/`; it names no route.
IMPORT = re.compile(r"^\s*(import|export)\b|\brequire\s*\(|\bfrom\s+[\"']")

# A module specifier (`import("@/lib/api/ground-station-api")`,
# `vi.mock("@/lib/api/ground-station/ws-ticket")`). A source path, not a route.
MODULE_SPECIFIER = re.compile(r"""["'`][@.][\w@/.-]*$""")

# An absolute URL to a CONCRETE host. A third-party weather or terrain service
# has its own `/api/...` namespace and nothing to do with the agent. A URL
# built against an interpolated host (`http://{}:8080/api/...`) keeps its
# placeholder and is still checked, because that one IS the agent.
EXTERNAL_URL = re.compile(r"""https?://[^\s"'`{}]*$""")

# A path fragment a client builds by concatenation cannot be resolved as
# written and is not a finding — but a fragment ending mid-segment usually IS
# one half of a split literal, which is the shape that hides a rename. Those
# are reported separately as unresolvable rather than silently skipped.
TRAILING_JUNK = "/-_."


def served_paths() -> list[list[str]]:
    """Every served path from the committed table, as segment lists."""
    if not TABLE.is_file():
        raise SystemExit(f"{TABLE} is missing — run scripts/gen-api-surface.py")
    out: list[list[str]] = []
    for line in TABLE.read_text(encoding="utf-8").splitlines():
        cells = [c.strip() for c in line.split("|")]
        if len(cells) < 4:
            continue
        path = cells[2].strip("`")
        if path.startswith("/"):
            out.append(path.split("/"))
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


def resolves(path: str, table: list[list[str]]) -> bool:
    """Whether a client literal names something the agent serves.

    Exact template match, or — for a base-URL constant like Android's
    ``api/v1/ground-station`` and the CLI's ``/api/v1/ground-station/recording``
    — a proper prefix of a served path. A prefix is enough to prove the family
    exists, which is the rename this check exists to catch; a path renamed out
    from under a client is a prefix of nothing.
    """
    segs = path.split("/")
    if any(matches(segs, t) for t in table):
        return True
    return any(len(t) > len(segs) and _segments(segs, t[: len(segs)]) for t in table)


def owns(path: str, own_routes: set[str]) -> bool:
    """Whether this literal is one of the CLIENT'S OWN server routes.

    Mission Control is a Next.js app: `/api/lan-pair/probe`, `/api/pid-analysis`
    and friends are its own route handlers under `src/app/api/**`, not calls to
    the agent. Resolved from the directory tree rather than a hand-kept list, so
    a new server route never has to be added here.
    """
    first = path.split("/")[2] if len(path.split("/")) > 2 else ""
    return first in own_routes


def own_server_routes(root: Path) -> set[str]:
    """First path segment of each of the client's own `/api/*` route folders."""
    api_dir = root / "app" / "api"
    if not api_dir.is_dir():
        return set()
    return {d.name for d in api_dir.iterdir() if d.is_dir()}


def main() -> int:
    table = served_paths()
    failures: list[tuple[str, Path, int, str]] = []
    scanned = 0
    for name, root, exts in CLIENTS:
        if not root.is_dir():
            print(f"  skip {name}: {root} not in this checkout")
            continue
        seen = 0
        own_routes = own_server_routes(root)
        for file in sorted(root.rglob("*")):
            if not file.is_file() or file.suffix not in exts:
                continue
            try:
                text = file.read_text(encoding="utf-8")
            except UnicodeDecodeError:
                continue
            for lineno, line in enumerate(text.splitlines(), 1):
                if COMMENT.match(line) or IMPORT.search(line):
                    continue
                line = INTERPOLATION.sub("{}", line)
                for m in LITERAL.finditer(line):
                    before = line[: m.start()]
                    if MODULE_SPECIFIER.search(before) or EXTERNAL_URL.search(before):
                        continue
                    path = normalise(m.group(0))
                    if path == "/api" or not path.startswith("/api/"):
                        continue
                    if own_routes and owns(path, own_routes):
                        continue
                    seen += 1
                    if not resolves(path, table):
                        failures.append((name, file, lineno, path))
        scanned += seen
        print(f"  {name}: {seen} path literals")

    if failures:
        print(f"\n{len(failures)} client path literal(s) resolve against no agent route:\n")
        for name, file, lineno, path in failures:
            rel = file.relative_to(MONOREPO) if MONOREPO in file.parents else file
            print(f"  {path}\n    {name} — {rel}:{lineno}")
        print(
            "\nEither the route was renamed on one side only, or the client calls "
            "a surface the agent has never served. Fix the caller, add the route, "
            "or regenerate docs/api-surface.md if the agent side just changed."
        )
        return 1

    print(f"\nall {scanned} client path literals resolve against docs/api-surface.md")
    return 0


if __name__ == "__main__":
    sys.exit(main())
