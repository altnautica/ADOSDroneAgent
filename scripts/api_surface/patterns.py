"""Paths, client roots and the regexes the API surface scan reads call sites with."""

from __future__ import annotations

import re
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
MONOREPO = REPO.parent
TABLE = REPO / "docs" / "api-surface.md"

# Where each client's source lives, and which file extensions carry path
# literals. A client absent from a partial checkout (the agent repo alone, or
# CI's two side-by-side checkouts) is skipped with a note, so the check stays
# runnable there. Inside the full monorepo checkout (``.gitmodules`` present
# beside this repo) every client is expected, and a missing root fails the
# check: a moved client directory would otherwise silently stop being scanned.
# The Android app is a multi-module Gradle build: ``app``, ``core/*`` and
# ``feature/*`` each carry sources.
CLIENTS = [
    ("Mission Control", MONOREPO / "ADOSMissionControl" / "src", {".ts", ".tsx"}),
    ("Android GCS app", MONOREPO / "ADOSAndroidGCS" / "app" / "src", {".kt", ".java"}),
    ("Android GCS core", MONOREPO / "ADOSAndroidGCS" / "core", {".kt", ".java"}),
    ("Android GCS feature", MONOREPO / "ADOSAndroidGCS" / "feature", {".kt", ".java"}),
    ("ADOS-MCP", MONOREPO / "ADOS-MCP" / "src", {".ts"}),
    ("agent dashboard", REPO / "dashboard" / "src", {".ts", ".tsx"}),
    ("agent cockpit", REPO / "cockpit" / "src", {".ts", ".tsx"}),
    ("ados CLI", REPO / "src" / "ados" / "cli", {".py"}),
    ("ados TUI", REPO / "crates" / "ados-tui" / "src", {".rs"}),
    # The REST client module only: the rest of the protocol crate is IPC, relay
    # and codec code whose `/api/...` strings are forwarded paths, not calls.
    ("Rust REST client", REPO / "crates" / "ados-protocol" / "src" / "rest.rs", {".rs"}),
]

# Present only in the full monorepo checkout.
MONOREPO_MARKER = MONOREPO / ".gitmodules"

# Build output directories inside a client root: generated, not call sites.
BUILD_DIRS = {"build", ".gradle", "node_modules"}

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
INTERPOLATION = re.compile(r"\$\{[^{}]*(?:\{[^{}]*\}[^{}]*)*\}|\{[A-Za-z_][\w.]*\([^{}()]*\)\}")

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

# The Android client (Kotlin) names a route relative to the agent root, with no
# leading slash: `get("api/status", ...)`, `sendJson("POST", "api/...")`, and
# `"$BASE/wfb"` against a file-local `const val BASE = "api/..."`. Each string
# literal that is wholly such a path is rewritten to the slash-led form the scan
# reads, a file-local path constant is expanded in place, and a bare `$name`
# becomes `${name}` so it collapses to one placeholder segment like any other
# interpolation (`"api/plugins/${seg(id)}/gcs/$encoded"` matches the
# `{*asset_path}` route by its literal prefix).
KOTLIN_EXTS = {".kt", ".java"}
KOTLIN_STRING = re.compile(r'"((?:\\.|\$\{[^{}]*(?:\{[^{}]*\}[^{}]*)*\}|[^"\\])*)"')
KOTLIN_CONST = re.compile(r'\bval\s+([A-Z][A-Z0-9_]*)\s*=\s*"((?:api/|whep|hls/)[^"$]*)"')
KOTLIN_PATH = re.compile(r"^(?:api/|whep(?:/|$)|hls/)[^\s]*$")
KOTLIN_REF = re.compile(r"\$(?:\{([A-Za-z_]\w*)\}|([A-Za-z_]\w*))")
# The verb a Kotlin call site states beyond `"POST", "api/..."` on one line:
# the client's own helpers (`get(path)` and `bytes(path)` are GETs, the plugin
# API's `post(path)` a POST), or a verb argument alone on the line above the
# path in a call split across lines (`send(\n "PUT",\n "$BASE/wfb", ...`).
KOTLIN_CALL = re.compile(r"""(?<!\w)(get|post|bytes)\s*\(\s*"$""")
KOTLIN_PREV_VERB = re.compile(r"""^\s*"(GET|POST|PUT|PATCH|DELETE)"\s*,\s*$""")
