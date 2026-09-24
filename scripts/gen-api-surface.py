#!/usr/bin/env python3
"""Generate ``docs/api-surface.md``: every HTTP route the agent serves.

There is no single place today that states which agent route each client
depends on, so a route rename lands as a silent 404 in one of five clients.
This writes that place, from the two producers rather than from a hand-kept
list:

* the native control front -- ``crates/ados-control/src/routing.rs``
  ``native_routes()``, the auth edge's own source of truth, dumped as JSON by
  ``cargo run -p ados-control --example api-surface``;
* the residual FastAPI surface -- the routers ``src/ados/api/server.py``
  mounts, enumerated through the generated OpenAPI schema plus a walk for the
  WebSocket routes OpenAPI does not describe.

Run it after adding, removing or renaming a route:

    ADOSDroneAgent/.venv/bin/python scripts/gen-api-surface.py

Two guards keep the committed file honest without anyone remembering to run
this: ``crates/ados-control/tests/api_surface.rs`` fails when the native half
drifts from ``native_route_table()``, and ``scripts/check-api-surface.py``
fails when a client calls a path the table does not carry.
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
OUT = REPO / "docs" / "api-surface.md"

# Routers mounted under ``/api`` by ``ados.api.server.create_app``, in the same
# order, plus the one mounted at root. Kept beside that function: a router
# added there and not here is a surface the table would silently omit, which
# ``check-api-surface.py`` then reports as an unresolvable client path.
API_ROUTERS = [
    "video",
    "pairing",
    "setup",
    "dashboard",
    "display",
    "peripherals",
    "peripherals_v1",
    "vision_models",
    "vision_detections",
    "ground_station",
    "network",
    "plugins",
]
ROOT_ROUTERS = ["whep"]

# The logging store's own query edge. A separate listener on its own port, not
# part of either HTTP front, but a real surface two clients dial -- Mission
# Control's `direct` log tier and `ados logs`.
LOGD_PORT = 8090
LOGD_PATHS = [
    "/v1/query",
    "/v1/tail",
    "/v1/aggregate",
    "/v1/export",
    "/v1/sessions",
    "/v1/stats",
    "/v1/healthz",
    "/v1/openapi.json",
]

# Paths served with no credential at all, by design. Recorded beside the route
# so a reader does not have to cross-reference `auth::is_public` to learn that
# a mutating route answers an unauthenticated caller.
UNAUTHENTICATED = {
    ("GET", "/healthz"),
    ("GET", "/api/ping"),
    ("GET", "/api/pairing/info"),
    ("GET", "/api/pairing/code"),
    ("POST", "/api/pairing/claim"),
    ("GET", "/api/version"),
    ("GET", "/api/dashboard/pin/status"),
    ("POST", "/api/dashboard/pin/verify"),
    ("POST", "/api/dashboard/pin/set"),
    ("WS", "/api/v1/ground-station/ws/uplink"),
    ("WS", "/api/v1/ground-station/pic/events"),
    ("WS", "/api/v1/ground-station/ws/mesh"),
    ("WS", "/api/v1/ground-station/ws/buttons"),
}


def native_routes() -> list[tuple[str, str]]:
    """``(method, path)`` for every route the native front serves itself."""
    proc = subprocess.run(
        ["cargo", "run", "--quiet", "-p", "ados-control", "--example", "api-surface"],
        cwd=REPO / "crates",
        capture_output=True,
        text=True,
        check=False,
    )
    if proc.returncode != 0:
        sys.stderr.write(proc.stderr)
        raise SystemExit("dumping the native route table failed")
    rows = json.loads(proc.stdout)
    return [(r["method"], r["path"]) for r in rows]


def _walk(router, prefix: str, out: list[tuple[str, str]]) -> None:
    """Collect ``(method, path)`` from a router tree, WebSockets included.

    A WebSocket route is recorded as ``WS``, not ``GET``: it answers only an
    upgrade, so a plain HTTP GET against it matches no route and 404s. Listing
    it as GET let the client check clear exactly that call.
    """
    for route in getattr(router, "routes", []):
        inner = getattr(route, "original_router", None)
        if inner is not None:
            ctx = getattr(route, "include_context", None)
            _walk(inner, prefix + getattr(ctx, "prefix", ""), out)
            continue
        path = getattr(route, "path", None)
        if not path:
            continue
        full = wire_path(prefix + path)
        methods = getattr(route, "methods", None)
        if not methods:
            out.append(("WS", full))
            continue
        for method in sorted(methods):
            if method in ("HEAD", "OPTIONS"):
                continue
            out.append((method, full))


def residual_routes() -> list[tuple[str, str]]:
    """``(method, path)`` for every route the residual FastAPI surface serves.

    Walks the routers rather than reading the generated OpenAPI schema: that
    schema omits WebSocket routes entirely (four of the ground station's live
    surfaces are upgrades) and rewrites a tail-swallowing ``{name:path}``
    parameter to a plain ``{name}``, which would make
    a tail route unresolvable against its own path.
    """
    from ados.api import routes as routes_pkg

    out: list[tuple[str, str]] = []
    for name in API_ROUTERS:
        module = getattr(routes_pkg, name, None) or __import__(
            f"ados.api.routes.{name}", fromlist=["router"]
        )
        _walk(module.router, "/api", out)
    for name in ROOT_ROUTERS:
        module = getattr(routes_pkg, name, None) or __import__(
            f"ados.api.routes.{name}", fromlist=["router"]
        )
        _walk(module.router, "", out)
    return out


def wire_path(path: str) -> str:
    """Normalise a FastAPI path to the template vocabulary the table uses.

    FastAPI spells a tail-swallowing parameter ``{name:path}``; the native
    router and this table spell it ``{*name}``. One vocabulary, so a consumer
    of the table does not have to know which producer a row came from.
    """
    return re.sub(r"\{(\w+):path\}", r"{*\1}", path)


def table(rows: list[tuple[str, str, str]]) -> list[str]:
    lines = ["| Method | Path | Notes |", "| --- | --- | --- |"]
    for method, path, note in rows:
        lines.append(f"| {method} | `{path}` | {note} |")
    return lines


def note_for(method: str, path: str) -> str:
    """The posture facts a reader would otherwise have to cross-reference.

    Both can be true of one route and both matter: `POST /api/dashboard/pin/set`
    answers an unauthenticated caller by design (trust-on-first-use) AND is
    refused over the radio relay.
    """
    notes = []
    if (method, path) in UNAUTHENTICATED:
        notes.append("unauthenticated by design")
    if path in RELAY_FORBIDDEN:
        notes.append("relay-forbidden")
    return "; ".join(notes)


def shadow_native(
    native: list[tuple[str, str]], residual: list[tuple[str, str]]
) -> list[tuple[str, str]]:
    """Drop the residual copies of routes the front answers itself.

    A route the front serves natively is answered by the front even when the
    residual also declares it, so the residual copy is recorded as shadowed
    rather than listing one route twice with two different owners.

    Keyed on ``(method, path)``, NOT on path alone. ``routing::is_native`` is
    method-scoped, so one path can be split between the two producers:
    ``GET /api/video/config`` is native while ``POST /api/video/config`` is
    proxied to the residual. Shadowing by path erased that POST from the table
    entirely — the table asserted the front owned a route it forwards, and the
    client calling the write had no row of its own to resolve against. The
    failure mode is invisible from the output: a table missing a row looks
    exactly like a complete one.
    """
    native_pairs = set(native)
    return [(m, p) for m, p in residual if (m, p) not in native_pairs]


def main() -> int:
    native = sorted(set(native_routes()), key=lambda r: (r[1], r[0]))
    residual = sorted(set(residual_routes()), key=lambda r: (r[1], r[0]))
    residual = shadow_native(native, residual)

    body: list[str] = [
        "# Agent HTTP surface",
        "",
        "Every route the agent serves, and who serves it. Generated -- do not",
        "hand-edit. Regenerate with:",
        "",
        "```",
        "ADOSDroneAgent/.venv/bin/python scripts/gen-api-surface.py",
        "```",
        "",
        "Two guards keep it true. `crates/ados-control/tests/api_surface.rs`",
        "fails when the native section drifts from `routing.rs`'s",
        "`native_route_table()`. `scripts/check-api-surface.py` fails when any",
        "client calls a path this table does not carry, which is what turns a",
        "one-sided route rename from a silent 404 into a build failure.",
        "",
        "A `{name}` segment matches one path segment; `{*name}` swallows the",
        "tail. Method `WS` marks a route that answers only a WebSocket upgrade;",
        "a plain HTTP request to it is not served.",
        "",
        "## Native — `ados-control` on :8080",
        "",
        "The front answers these itself. They take its own auth lane: rate",
        "limiter, pairing gate, MCP-scope admission.",
        "",
    ]
    body += table([(m, p, note_for(m, p)) for m, p in native])
    body += [
        "",
        f"{len(native)} native routes.",
        "",
        "## Residual — FastAPI behind the front's proxy, same :8080",
        "",
        "The front forwards these to the residual Python over its internal Unix",
        "socket, authenticating them on the proxied lane first. A path under a",
        "`PERMANENT_PYTHON_PREFIXES` prefix answers `501` when the residual is",
        "absent (a known feature, not on this profile) rather than `404`.",
        "",
    ]
    body += table([(m, p, note_for(m, p)) for m, p in residual])
    body += [
        "",
        f"{len(residual)} residual routes.",
        "",
        f"## Logging store — `ados-logd` on :{LOGD_PORT}",
        "",
        "A separate listener on its own port, dialled directly by Mission",
        "Control's `direct` log tier and by `ados logs`. Not reachable through",
        "either HTTP front.",
        "",
    ]
    body += table([("GET", p, "") for p in LOGD_PATHS])
    body += [
        "",
        "## Not an HTTP route",
        "",
        "| Surface | Where |",
        "| --- | --- |",
        "| MAVLink WebSocket | `ws://<host>:8765/` — `ados-mavlink-router`, "
        "ticket or `X-ADOS-Key` |",
        "| Relayed drone surface | "
        "`/api/v1/ground-station/relay-proxy/{peer_device_id}/{*path}` — the "
        "`{*path}` is the drone's own path from this table |",
        "",
    ]
    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text("\n".join(body) + "\n", encoding="utf-8")
    print(f"wrote {OUT.relative_to(REPO)}: {len(native)} native, {len(residual)} residual")
    return 0


# Mirrors `auth::RELAY_FORBIDDEN_PATHS`. The Rust test
# `every_denylisted_path_is_a_route_something_actually_serves` reads this file
# back and asserts each of those literals appears above, so a denylist entry
# that matches no served path fails the build instead of reading as protection.
RELAY_FORBIDDEN = {
    "/api/pairing/unpair",
    "/api/pairing/accept",
    "/api/mcp/tokens",
    "/api/mcp/revoke",
    "/api/dashboard/pin/set",
    "/api/dashboard/pin/clear",
    "/api/wfb/pair/local-bind",
    "/api/wfb/pair/unpair",
    "/api/v1/ground-station/wfb/pair",
    "/api/plugins/install",
    "/api/plugins/install_from_url",
    "/api/plugins/capability-token",
    "/api/plugins/{plugin_id}/grant",
    "/api/plugins/{plugin_id}/enable",
    "/api/services/{name}/restart",
    "/api/mavlink/signing/disable-on-fc",
    "/api/v1/setup/reset",
    "/api/v1/setup/reboot",
    "/api/v1/setup/cloud-choice",
    "/api/v1/setup/remote-access/cloudflare",
    "/api/v1/system/restart-supervisor",
    "/api/v1/ground-station/factory-reset",
}


if __name__ == "__main__":
    raise SystemExit(main())
