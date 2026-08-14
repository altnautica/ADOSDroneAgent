# SPDX-License-Identifier: GPL-3.0-only
# Copyright (C) 2026 Altnautica — ADOS Drone Agent
"""A `Mirrors <symbol>` citation must name a symbol that still exists.

Each wave of the Rust migration wrote these to document the equivalence it was
maintaining, then deleted the predecessor once its bench gate passed. The comment
outlived the thing it pointed at 132 times before this gate existed.

Why it matters at the point of change: the comment asserts that a SECOND
implementation exists which your edit must stay byte-identical to. A reader either
wastes time hunting a file that is not there, or — worse — treats the Rust as a
copy rather than the authority and edits defensively around a constraint that no
longer binds.

Resolution is deliberately loose: the identifier need only appear as a whole word
somewhere under `src/ados/`. A tighter check (a `def`/`class` at module level)
would fire on a method, a re-export, or a name that is also a dict key, and a gate
with false positives gets deleted. This one fires only when the symbol is gone
from the Python tree entirely, which is exactly the defect.

The word carries the claim: `Mirrors` means a Python authority exists. A comment
pointing at a shell function, a sibling Rust item or the browser connector says
"matches", "follows" or "parallels", so this gate resolves every `Mirrors` it finds
against the Python tree and never needs an allowlist of exceptions.
"""

from __future__ import annotations

import re
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
CRATES = REPO_ROOT / "crates"
PYTHON_SRC = REPO_ROOT / "src" / "ados"

#: `Mirrors ... \`symbol\`` — the citation form, capturing the first backticked
#: identifier after the word.
CITATION = re.compile(r"Mirrors[^`\n]*`([A-Za-z_][A-Za-z0-9_]*)`")

#: A Rust line comment of any flavour: `///`, `//!` or plain `//`.
COMMENT_MARKER = re.compile(r"^\s*(?:///|//!|//)\s?")

#: A scan finding fewer than this many citations has a broken extractor, not a
#: clean tree. The floor is the count that survived a citation-by-citation sweep of
#: the whole crate tree, in which every citation was judged against reality rather
#: than against this gate: the ones whose predecessor had been deleted went, the
#: ones naming something outside `src/ados/` were reworded off the word, and the
#: rest were confirmed to still name the authority. Most of them were only
#: reachable once the extractor started reading wrapped comments, which had hidden
#: 70 dangling citations behind a clean result. The survivors point at the
#: permanently-Python layers (vision, scripting, HAL bootstrap, the plugin runtime,
#: the SDK), so the count only falls as those migrate, deliberately and never
#: silently. It sits below the sweep's own tally because several comments were
#: rewritten after it, in the same pass, and a citation went with each. Lowering
#: it is a deliberate act and belongs in the same commit as the deletion.
MIN_CITATIONS = 163


def _rust_sources() -> list[Path]:
    """Every crate source, excluding build artifacts under `target/` (which
    contain the same strings inside `.rmeta` and would be scanned forever)."""
    return [
        p for p in CRATES.rglob("*.rs") if "target" not in p.relative_to(CRATES).parts
    ]


def _python_text() -> str:
    return "\n".join(
        p.read_text(errors="ignore")
        for p in PYTHON_SRC.rglob("*.py")
        if "__pycache__" not in p.parts
    )


def _comment_blocks(path: Path) -> list[tuple[int, str]]:
    """Each run of adjacent comment lines, joined into one string.

    Scanning line by line misses every citation the formatter wrapped, and it
    wraps most of them: `Mirrors` lands at the end of one line and the backticked
    symbol at the start of the next. That blind spot hid 70 dangling citations
    from the first version of this gate while it reported a clean tree, so the
    extractor reads a whole comment block at a time. A non-comment line ends the
    block, so a citation can never be assembled across an intervening statement.

    The reported line is the block's first line, which is where a reader starts.
    """
    blocks: list[tuple[int, str]] = []
    start: int | None = None
    parts: list[str] = []
    for lineno, line in enumerate(path.read_text(errors="ignore").splitlines(), 1):
        marker = COMMENT_MARKER.match(line)
        if marker:
            if start is None:
                start = lineno
            parts.append(line[marker.end() :])
            continue
        if start is not None:
            blocks.append((start, " ".join(parts)))
            start, parts = None, []
    if start is not None:
        blocks.append((start, " ".join(parts)))
    return blocks


def test_every_mirrors_citation_names_a_symbol_that_exists() -> None:
    sources = _rust_sources()
    assert len(sources) >= 100, (
        f"scanned only {len(sources)} crate sources; the walk is broken, so its "
        "clean result means nothing"
    )

    python = _python_text()
    assert len(python) > 100_000, "the Python tree read is empty or truncated"

    resolved: dict[str, bool] = {}
    citations = 0
    dangling: list[str] = []

    for path in sources:
        rel = path.relative_to(REPO_ROOT)
        for lineno, block in _comment_blocks(path):
            for match in CITATION.finditer(block):
                citations += 1
                symbol = match.group(1)
                if symbol not in resolved:
                    resolved[symbol] = (
                        re.search(rf"\b{re.escape(symbol)}\b", python) is not None
                    )
                if not resolved[symbol]:
                    dangling.append(f"  {rel}:{lineno}  `{symbol}`")

    assert citations >= MIN_CITATIONS, (
        f"extracted only {citations} citations; the pattern stopped matching, so "
        "this gate is asserting nothing. Fix the extractor before lowering the floor."
    )

    assert not dangling, (
        "these `Mirrors` citations name a symbol that no longer exists in "
        "src/ados/:\n"
        + "\n".join(sorted(dangling))
        + "\nThe comment asserts a second implementation your edit must match. "
        "When the predecessor is deleted the citation goes with it — the Rust is "
        "the authority now, so say what the code does instead of what it copies."
    )


#: A cited Python file path inside a `Mirrors` sentence, e.g.
#: ``Mirrors the Python `VehicleState` (services/mavlink/state.py)``.
CITED_PATH = re.compile(r"Mirrors[^\n]*?\(([A-Za-z0-9_./-]+\.py)\)")

#: Only a `Mirrors` sentence claims a Python authority. A cross-language
#: reference — a shell function, a sibling Rust item, the browser connector — says
#: "matches" / "follows" / "parallels" instead, so this gate never has to carry an
#: allowlist of things it is not allowed to resolve.


def test_every_cited_python_file_still_exists() -> None:
    """A citation can name a symbol that survives and a file that does not.

    `VehicleState` is the case that motivated this: the name lives on as an alias
    in `services/mqtt/gateway.py`, so the symbol check resolves, while the cited
    `services/mavlink/state.py` had been deleted — sending a reader to a path that
    is not there, which is the same waste in a smaller way.
    """
    missing: list[str] = []
    cited = 0
    for path in _rust_sources():
        rel = path.relative_to(REPO_ROOT)
        for lineno, block in _comment_blocks(path):
            for match in CITED_PATH.finditer(block):
                cited += 1
                target = match.group(1)
                candidates = (
                    PYTHON_SRC / target,
                    REPO_ROOT / "src" / target,
                    REPO_ROOT / target,
                )
                if not any(c.exists() for c in candidates):
                    missing.append(f"  {rel}:{lineno}  {target}")

    assert not missing, (
        "these `Mirrors` citations point at a Python file that no longer "
        "exists:\n" + "\n".join(sorted(missing))
    )
