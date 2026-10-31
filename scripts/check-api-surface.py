#!/usr/bin/env python3
"""Fail when a client calls an agent route the agent does not serve.

There are seven clients of the agent's HTTP surface — Mission Control, the
Android GCS, ADOS-MCP, the on-device dashboard SPA, the ``ados`` CLI, the
``ados`` terminal UI and the shared Rust REST client it uses — and
no build step ever compared any of them against the agent. A route renamed on
one side therefore landed as a silent 404 in the others, discovered by an
operator rather than by CI. Two such holes were live when this was written:
four ``/api/ota*`` paths nothing has ever served, and an Android MAVLink
WebSocket path with no producer.

This scans each client's sources for ``/api/...``, ``/whep`` and ``/hls/...``
path literals (the Android client's root-relative ``"api/..."`` strings
included) and resolves every one against ``docs/api-surface.md``. A literal
that resolves against no served route is an error. Where the call site states
its HTTP method (``method: "PUT"``, ``request("POST", ...)``, ``client.put(``,
a WebSocket URL), the (method, path) pair must be served too: a route that
exists only for another method answers 405, which is the same silent break.

    ADOSDroneAgent/.venv/bin/python scripts/check-api-surface.py

Exit 0 when every literal resolves, 1 otherwise. Needs no toolchain beyond
CPython: it reads the committed table, so it runs before anything is built.
"""

from __future__ import annotations

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from api_surface.kotlin import kotlin_method, kotlin_paths  # noqa: E402
from api_surface.patterns import INTERPOLATION, LITERAL  # noqa: E402
from api_surface.routes import method_served, normalise, owns, resolves, websocket_only  # noqa: E402
from api_surface.scan import main  # noqa: E402
from api_surface.sites import stated_method  # noqa: E402

__all__ = [
    "INTERPOLATION",
    "LITERAL",
    "kotlin_method",
    "kotlin_paths",
    "main",
    "method_served",
    "normalise",
    "owns",
    "resolves",
    "stated_method",
    "websocket_only",
]

if __name__ == "__main__":
    sys.exit(main())
