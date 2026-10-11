#!/usr/bin/env python3
"""Ratchet the size of every Rust source file under ``crates/``.

A file's size is its line count before the first ``#[cfg(test)]`` line (the
whole file when it has none), so inline test modules never count against it.

- A file at or under ``LIMIT`` lines passes.
- A file over ``LIMIT`` must be listed in ``crates/.loc-baseline`` and may not
  grow past its recorded count. Shrinking is always fine; ``--update`` lowers
  the recorded counts (and drops files that fell under the limit).
- Generated files are exempt.

    python scripts/check-rust-size.py            # check, exit 1 on a violation
    python scripts/check-rust-size.py --update   # rewrite the baseline

``--update`` never raises a recorded count and never adds a new file: growth
is fixed by splitting the file, not by editing the baseline. ``--update
--accept-growth`` exists only to seed a baseline from scratch.
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
CRATES = REPO / "crates"
BASELINE = CRATES / ".loc-baseline"
LIMIT = 500

# Generated sources, by file name. They are rewritten by a generator and their
# size follows the data they encode.
GENERATED = {"capabilities.rs", "dispatch.rs", "wfb_tables.rs", "contracts.rs", "paths.rs"}
SKIP_DIRS = {"target", ".git"}
TEST_MARKER = "#[cfg(test)]"


def measure(path: Path) -> int:
    count = 0
    with path.open(encoding="utf-8", errors="replace") as fh:
        for line in fh:
            if line.strip().startswith(TEST_MARKER):
                break
            count += 1
    return count


def sources(root: Path) -> dict[str, int]:
    out: dict[str, int] = {}
    for path in sorted(root.rglob("*.rs")):
        rel = path.relative_to(root)
        if any(part in SKIP_DIRS for part in rel.parts) or path.name in GENERATED:
            continue
        out[rel.as_posix()] = measure(path)
    return out


def read_baseline(path: Path) -> dict[str, int]:
    out: dict[str, int] = {}
    if not path.is_file():
        return out
    for raw in path.read_text(encoding="utf-8").splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        count, rel = line.split(None, 1)
        out[rel] = int(count)
    return out


def write_baseline(path: Path, counts: dict[str, int]) -> None:
    lines = [
        "# Rust files over the size limit, with their line count before the first",
        "# #[cfg(test)]. A listed file may shrink but never grow; an unlisted file",
        f"# must stay at or under {LIMIT} lines. Maintained by scripts/check-rust-size.py.",
    ]
    lines += [f"{counts[rel]} {rel}" for rel in sorted(counts)]
    path.write_text("\n".join(lines) + "\n", encoding="utf-8", newline="\n")


def check(current: dict[str, int], baseline: dict[str, int]) -> list[str]:
    errors = []
    for rel, count in current.items():
        if count <= LIMIT:
            continue
        allowed = baseline.get(rel)
        if allowed is None:
            errors.append(f"{rel}: {count} lines, limit {LIMIT} (new or unlisted file)")
        elif count > allowed:
            errors.append(f"{rel}: {count} lines, grew past its baseline of {allowed}")
    return errors


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--update", action="store_true", help="lower the baseline to current counts")
    ap.add_argument("--accept-growth", action="store_true", help="with --update: seed from scratch")
    ap.add_argument("--root", type=Path, default=CRATES, help=argparse.SUPPRESS)
    ap.add_argument("--baseline", type=Path, default=BASELINE, help=argparse.SUPPRESS)
    args = ap.parse_args(argv)

    current = sources(args.root)
    baseline = read_baseline(args.baseline)

    if args.update:
        kept = {}
        for rel, count in current.items():
            if count <= LIMIT:
                continue
            if args.accept_growth:
                kept[rel] = count
            elif rel in baseline:
                kept[rel] = min(count, baseline[rel])
        write_baseline(args.baseline, kept)
        print(f"baseline: {len(kept)} file(s) over {LIMIT} lines recorded")
        if not args.accept_growth:
            errors = check(current, kept)
            for e in errors:
                print(f"  {e}")
            return 1 if errors else 0
        return 0

    errors = check(current, baseline)
    if errors:
        print(f"{len(errors)} Rust file(s) over the size ratchet:\n")
        for e in errors:
            print(f"  {e}")
        print(f"\nSplit the file. New files stay at or under {LIMIT} lines before #[cfg(test)].")
        return 1
    print(f"PASS: {len(current)} Rust files within the size ratchet ({len(baseline)} baselined)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
