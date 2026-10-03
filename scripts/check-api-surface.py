#!/usr/bin/env python3
"""Fail when a client calls an agent route the agent does not serve.

There are five clients of the agent's HTTP surface — Mission Control, the
Android GCS, ADOS-MCP, the on-device dashboard SPA and the ``ados`` CLI — and
no build step ever compared any of them against the agent. A route renamed on
one side therefore landed as a silent 404 in the others, discovered by an
operator rather than by CI. Two such holes were live when this was written:
four ``/api/ota*`` paths nothing has ever served, and an Android MAVLink
WebSocket path with no producer.

This scans each client's sources for ``/api/...``, ``/whep`` and ``/hls/...``
path literals and resolves every one against ``docs/api-surface.md``. A literal
that resolves against no served route is an error. Where the call site states
its HTTP method (``method: "PUT"``, ``request("POST", ...)``, ``client.put(``,
a WebSocket URL), the (method, path) pair must be served too: a route that
exists only for another method answers 405, which is the same silent break.

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
LITERAL = re.compile(r"/(?:api/|whep\b|hls/)[A-Za-z0-9/_.{}:*<>-]*")

# The HTTP method a call site states, read from the text that follows the
# literal within the same call (`fetch(url, { method: "PUT" })`) ...
METHOD_OPTION = re.compile(r"""\bmethod\s*[:=]\s*["'`](GET|POST|PUT|PATCH|DELETE)["'`]""", re.I)
# ... or from the text on the same line before it: a verb argument
# (`request("POST", `${base}/api/...`)`) or a verb-named client method
# (`client.put(f"{base}/api/...")`).
METHOD_ARG = re.compile(r"""["'](GET|POST|PUT|PATCH|DELETE)["']\s*,\s*[^,()]*$""", re.I)
METHOD_CALL = re.compile(r"""\.(get|post|put|patch|delete)\s*\(\s*[^,()]*$""", re.I)
# A WebSocket URL. The table lists a WebSocket route as `WS`, so one is checked
# as `WS`, and a plain HTTP call to a WebSocket-only route fails the check.
WEBSOCKET = re.compile(r"""(?:wss?://|new\s+WebSocket\s*\()[^;]*$""")
# Evidence that a call site opens a socket rather than fetching: a ws(s) URL,
# a WebSocket constructor, or an http->ws scheme rewrite.
SOCKET_HINT = re.compile(r"""wss?://|WebSocket|["'`]wss?:?["'`]""")
# How far past the literal to look for the call's options object.
METHOD_WINDOW_LINES = 6

# A client interpolation, with whatever call sits inside it
# (``${encodeURIComponent(id)}``, or a Python f-string's ``{_seg(plugin_id)}``).
# Collapsed to one placeholder segment before literals are extracted, because
# the regex above would otherwise stop at the opening paren and report half a
# path.
INTERPOLATION = re.compile(
    r"\$\{[^{}]*(?:\{[^{}]*\}[^{}]*)*\}|\{[A-Za-z_][\w.]*\([^{}()]*\)\}"
)

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

# A literal that stops at a separator (`"/api/" + x`, `"/api/plugins/"` handed
# to a builder) is half of a path the client assembles elsewhere. It cannot be
# resolved as written, and it is exactly the shape that hides a one-sided
# rename, so it is reported as unresolvable rather than skipped. Write the
# whole path as one template literal instead.
TRAILING_JUNK = "/-_."

# Base-URL constants a client appends sub-paths to. Only these may resolve as a
# proper prefix of a served route; any other literal must name a route exactly,
# so an unserved `GET /api/vision` cannot pass because `/api/vision/status`
# exists. Each entry is the normalised literal, and must still be a prefix of
# something the agent serves.
BASE_PREFIXES = {
    # Android's ground-station base and the CLI's recording base.
    "/api/v1/ground-station",
    "/api/v1/ground-station/recording",
    # Mission Control's relayed-node base (per peer and bare).
    "/api/v1/ground-station/relay-proxy",
    "/api/v1/ground-station/relay-proxy/{}",
    # Mission Control's observability proxy base.
    "/api/v2/observability",
}

# Client helpers that build an agent path from a fixed prefix plus a sub-path
# passed as an argument (`pluginUrl(pluginId, "attestation")`). The sub-path
# literal is joined to the prefix and resolved like any other literal; a call
# whose sub-path is not a literal cannot be checked and is reported.
HELPERS = {
    "pluginUrl": "/api/plugins/{}",
}
HELPER_CALL = re.compile(
    r"""(?<![\w.])(?:this\.)?(?P<name>""" + "|".join(HELPERS) + r""")\(\s*"""
    r"""[A-Za-z_][\w.]*\s*"""
    r"""(?:,\s*(?P<q>["'`])(?P<sub>(?:\$\{[^{}]*\}|(?!(?P=q))[^$\\])*)(?P=q)\s*)?\)"""
)
HELPER_USE = re.compile(r"""(?<![\w.])(?:this\.)?(?P<name>""" + "|".join(HELPERS) + r""")\(""")
HELPER_DECL = re.compile(r"""\b(?:private|public|protected|function|def|fun)\s+(?:async\s+)?$""")

# Test sources hold deliberately fake and malformed paths (traversal probes,
# `/api/x` placeholders) that no client ever sends; they are not call sites.
TEST_DIRS = {"__tests__", "__mocks__", "test", "tests", "androidTest"}
TEST_FILE = re.compile(r"(\.(test|spec)\.[cm]?[jt]sx?$)|(^test_.*\.py$)|(Test\.(kt|java)$)")


def is_test_source(file: Path, root: Path) -> bool:
    """Whether ``file`` is a test, by name or by a test directory under ``root``."""
    if TEST_FILE.search(file.name):
        return True
    return any(part in TEST_DIRS for part in file.relative_to(root).parts[:-1])


def logical_line(lines: list[str], index: int) -> tuple[str, int]:
    """``lines[index]`` joined with the lines an open interpolation spills onto.

    `` `/api/plugins/${encodeURIComponent(`` continued on the next lines is one
    path; read alone, the literal stops at the open ``${`` and looks truncated.
    Returns the joined text and the 1-based number of the last line it took.
    """
    text = lines[index]
    j = index
    while "${" in INTERPOLATION.sub("{}", text) and j + 1 < len(lines) and j - index < METHOD_WINDOW_LINES:
        j += 1
        text += lines[j].strip()
    return text, j + 1


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


def stated_method(before: str, after: str) -> str | None:
    """The HTTP method a call site states for the literal, or None."""
    if WEBSOCKET.search(before):
        return "WS"
    for pattern in (METHOD_ARG, METHOD_CALL):
        m = pattern.search(before)
        if m:
            return m.group(1).upper()
    # The options object belongs to this call only up to the next path literal.
    nxt = LITERAL.search(after)
    m = METHOD_OPTION.search(after[: nxt.start()] if nxt else after)
    return m.group(1).upper() if m else None


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


def unserved_method(
    path: str,
    before: str,
    rest: str,
    lines: list[str],
    lineno: int,
    line: str,
    tail: str,
    table: list[tuple[str, list[str]]],
) -> str | None:
    """The method a call site uses when ``path`` is not served for it, else None."""
    method = stated_method(before, rest + "\n" + tail)
    # A call that states no method is a plain fetch (GET) unless the site opens
    # a socket (the hint may sit a few lines either side: a
    # `subscribeWebSocket({` opener, a scheme chosen above the URL, a
    # `.replace(/^http/, "ws")`), which a WebSocket-only route needs.
    if method is None and websocket_only(path, table):
        head = "\n".join(lines[max(0, lineno - 1 - METHOD_WINDOW_LINES) : lineno - 1])
        site = head + "\n" + line + "\n" + tail
        method = "WS" if SOCKET_HINT.search(site) else "GET"
    if method and not method_served(path, method, table):
        return method
    return None


def main() -> int:
    table = served_paths()
    failures: list[tuple[str, Path, int, str]] = []
    fragments: list[tuple[str, Path, int, str]] = []
    method_failures: list[tuple[str, Path, int, str, str]] = []
    scanned = 0
    for name, root, exts in CLIENTS:
        if not root.is_dir():
            print(f"  skip {name}: {root} not in this checkout")
            continue
        seen = 0
        own_routes = own_server_routes(root)
        for file in sorted(root.rglob("*")):
            if not file.is_file() or file.suffix not in exts or is_test_source(file, root):
                continue
            try:
                text = file.read_text(encoding="utf-8")
            except UnicodeDecodeError:
                continue
            lines = text.splitlines()
            consumed = 0
            for lineno, raw_line in enumerate(lines, 1):
                # A line already read as the continuation of an open
                # interpolation above belongs to that call, not a new one.
                if lineno <= consumed:
                    continue
                if COMMENT.match(raw_line) or IMPORT.search(raw_line):
                    continue
                joined, consumed = logical_line(lines, lineno - 1)
                line = INTERPOLATION.sub("{}", joined)
                tail = INTERPOLATION.sub(
                    "{}", "\n".join(lines[consumed : consumed + METHOD_WINDOW_LINES])
                )

                # Paths built by a helper from a prefix and a sub-path argument.
                # Read from the line before interpolations collapse, because the
                # call usually sits inside one (`${this.pluginUrl(id, "x")}`).
                helper_spans: list[tuple[int, int]] = []
                for h in HELPER_CALL.finditer(joined):
                    helper_spans.append(h.span())
                    sub = h.group("sub") or ""
                    path = normalise(HELPERS[h.group("name")] + ("/" + sub if sub else ""))
                    seen += 1
                    if sub.endswith(tuple(TRAILING_JUNK)):
                        fragments.append((name, file, lineno, path + "/"))
                        continue
                    if not resolves(path, table):
                        failures.append((name, file, lineno, path))
                        continue
                    bad = unserved_method(
                        path,
                        joined[: h.start()],
                        INTERPOLATION.sub("{}", joined[h.end() :]),
                        lines,
                        lineno,
                        line,
                        tail,
                        table,
                    )
                    if bad:
                        method_failures.append((name, file, lineno, bad, path))
                for u in HELPER_USE.finditer(joined):
                    if HELPER_DECL.search(joined[: u.start()]):
                        continue
                    if not any(s <= u.start() < e for s, e in helper_spans):
                        seen += 1
                        fragments.append(
                            (name, file, lineno, f"{HELPERS[u.group('name')]}/<non-literal sub-path>")
                        )

                for m in LITERAL.finditer(line):
                    before = line[: m.start()]
                    if MODULE_SPECIFIER.search(before) or EXTERNAL_URL.search(before):
                        continue
                    raw = m.group(0)
                    path = normalise(raw)
                    if not path.startswith(("/api", "/whep", "/hls/")):
                        continue
                    if own_routes and owns(path, own_routes):
                        continue
                    seen += 1
                    # Half of a path assembled elsewhere: `"/api/" + x`, or a
                    # literal that stops at a separator or an interpolation
                    # that never closes. It names no route as written.
                    if path == "/api" or raw.endswith(tuple(TRAILING_JUNK)) or line[m.end() :].startswith("$"):
                        fragments.append((name, file, lineno, raw))
                        continue
                    if not resolves(path, table):
                        failures.append((name, file, lineno, path))
                        continue
                    bad = unserved_method(path, before, line[m.end() :], lines, lineno, line, tail, table)
                    if bad:
                        method_failures.append((name, file, lineno, bad, path))
        scanned += seen
        print(f"  {name}: {seen} path literals")

    if method_failures:
        print(f"\n{len(method_failures)} client call(s) use a method the route does not serve:\n")
        for name, file, lineno, method, path in method_failures:
            rel = file.relative_to(MONOREPO) if MONOREPO in file.parents else file
            print(f"  {method} {path}\n    {name} — {rel}:{lineno}")
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
    if fragments:
        print(f"\n{len(fragments)} client path(s) are assembled in pieces and cannot be resolved:\n")
        for name, file, lineno, path in fragments:
            rel = file.relative_to(MONOREPO) if MONOREPO in file.parents else file
            print(f"  {path}\n    {name} — {rel}:{lineno}")
        print(
            "\nWrite each agent path as one literal (a template literal with "
            "placeholders is fine) so a one-sided rename fails here, or, for a "
            "base the client appends to, add the base to BASE_PREFIXES."
        )
    if failures or fragments or method_failures:
        return 1

    print(f"\nall {scanned} client path literals resolve against docs/api-surface.md")
    return 0


if __name__ == "__main__":
    sys.exit(main())
