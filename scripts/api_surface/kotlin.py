"""Android (Kotlin) path literal rewriting and verb inference."""

from __future__ import annotations

import re

from api_surface.patterns import (
    INTERPOLATION,
    KOTLIN_CALL,
    KOTLIN_CONST,
    KOTLIN_PATH,
    KOTLIN_PREV_VERB,
    KOTLIN_REF,
    KOTLIN_STRING,
)


def kotlin_paths(lines: list[str]) -> list[str]:
    """``lines`` with every relative agent-path string made slash-led."""
    consts = {m.group(1): m.group(2) for line in lines for m in KOTLIN_CONST.finditer(line)}

    def expand(ref: re.Match[str]) -> str:
        return consts.get(ref.group(1) or ref.group(2), ref.group(0))

    def brace(ref: re.Match[str]) -> str:
        return "${" + ref.group(2) + "}" if ref.group(2) else ref.group(0)

    def rewrite(s: re.Match[str]) -> str:
        body = KOTLIN_REF.sub(expand, s.group(1))
        if not KOTLIN_PATH.match(INTERPOLATION.sub("{}", body)):
            return s.group(0)
        return '"/' + KOTLIN_REF.sub(brace, body) + '"'

    return [KOTLIN_STRING.sub(rewrite, line) for line in lines]


def kotlin_method(before: str, lines: list[str], lineno: int) -> str | None:
    """The verb a Kotlin helper call implies for the path that follows, or None."""
    m = KOTLIN_CALL.search(before)
    if m:
        return "GET" if m.group(1) == "bytes" else m.group(1).upper()
    if before.strip() == '"' and lineno >= 2:
        p = KOTLIN_PREV_VERB.match(lines[lineno - 2])
        if p:
            return p.group(1)
    return None
