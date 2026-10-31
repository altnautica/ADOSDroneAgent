"""Call-site helpers: test-source detection, logical lines and stated methods."""

from __future__ import annotations

import re
from pathlib import Path

from api_surface.patterns import (
    INTERPOLATION,
    LITERAL,
    METHOD_ARG,
    METHOD_CALL,
    METHOD_OPTION,
    METHOD_WINDOW_LINES,
    SOCKET_HINT,
    WEBSOCKET,
)
from api_surface.routes import method_served, websocket_only

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
    while (
        "${" in INTERPOLATION.sub("{}", text)
        and j + 1 < len(lines)
        and j - index < METHOD_WINDOW_LINES
    ):
        j += 1
        text += lines[j].strip()
    return text, j + 1


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
