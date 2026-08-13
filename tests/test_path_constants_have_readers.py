# SPDX-License-Identifier: GPL-3.0-only
# Copyright (C) 2026 Altnautica — ADOS Drone Agent
"""Every path constant must have a reader.

`ados/core/paths.py` is Python's path registry. A constant nothing reads is the
producer-with-no-reader defect, and it accumulates in a specific way here: a Rust service takes over a file, hardcodes its own literal (the contracts
registry is the cross-language source of truth, not this module), the last Python
reader is deleted — and the constant stays, still naming a real file, so it reads
as live to anyone grepping. Thirty-one had piled up by the time this gate was
written.

The check is deliberately loose about WHAT a reader is: any mention outside the
definition line counts, including a test or a script. The defect being caught is
"nothing anywhere refers to this", not "no production code refers to this".
"""

from __future__ import annotations

import re
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
PATHS_MODULE = REPO_ROOT / "src" / "ados" / "core" / "paths.py"

#: Where a reader might live. `data/` is included because a systemd unit or a
#: udev rule naming the same path is a reader of the concept, even though it
#: cannot import the constant.
SEARCH_ROOTS = ("src", "tests", "tools", "scripts", "data")

#: Extensions worth scanning. Anything else is a binary or a build artifact.
SEARCH_SUFFIXES = {".py", ".sh", ".service", ".rules", ".conf", ".yaml", ".yml", ".toml"}

#: A scan that finds fewer constants than this is broken, not proof of a clean
#: registry. Lower it deliberately (with a reason) if the module genuinely
#: shrinks that far; never to make a failure go away.
MIN_CONSTANTS = 60


def _constant_lines() -> dict[str, int]:
    """Every `NAME = ...` module-level constant, mapped to its 1-based line."""
    out: dict[str, int] = {}
    for lineno, line in enumerate(PATHS_MODULE.read_text().splitlines(), start=1):
        match = re.match(r"^([A-Z_0-9]+)\s*=", line)
        if match:
            out[match.group(1)] = lineno
    return out


def _sources() -> list[Path]:
    files: list[Path] = []
    for root in SEARCH_ROOTS:
        base = REPO_ROOT / root
        if not base.is_dir():
            continue
        files += [
            p
            for p in base.rglob("*")
            if p.is_file()
            and p.suffix in SEARCH_SUFFIXES
            and "__pycache__" not in p.parts
        ]
    return files


def test_every_path_constant_is_read_somewhere() -> None:
    constants = _constant_lines()
    assert len(constants) >= MIN_CONSTANTS, (
        f"found only {len(constants)} constants in {PATHS_MODULE.name}; the parse "
        "is broken, so its orphan list means nothing"
    )

    bodies: list[tuple[Path, str]] = []
    for path in _sources():
        try:
            bodies.append((path, path.read_text()))
        except (OSError, UnicodeDecodeError):
            continue
    assert len(bodies) > 200, (
        f"scanned only {len(bodies)} files; the walk is broken"
    )

    orphans: list[str] = []
    for name, lineno in constants.items():
        pattern = re.compile(rf"\b{name}\b")
        hits = 0
        for path, text in bodies:
            found = len(pattern.findall(text))
            if path == PATHS_MODULE:
                found -= 1  # its own definition line
            hits += found
        if hits <= 0:
            orphans.append(f"  paths.py:{lineno}  {name}")

    assert not orphans, (
        "these path constants have no reader anywhere in src/, tests/, tools/, "
        "scripts/ or data/:\n"
        + "\n".join(sorted(orphans))
        + "\nA constant that names a real file still reads as live to the next "
        "person grepping. Delete it, or wire the reader it was added for. If a "
        "Rust service owns the file now, the cross-language registry is "
        "crates/ados-protocol/contracts.toml, not this module."
    )
