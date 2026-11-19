#!/usr/bin/env python3
"""Guard the clock seam: code moved onto ADOS time stays on it.

A file listed as a ``seam`` in ``scripts/clock-seam-allow.txt`` runs its
control loops, timers and freshness checks on ADOS time (``ados_time`` /
``ados_protocol::simclock`` / ``ados_sdk::clock`` in Rust, ``ados.sdk.clock``
in Python), so a simulator running at N× drives it at N×. A wall-clock read
or timer in such a file would run at 1× underneath it. This guard fails on
any of these in a seam file:

* Rust: ``Instant::now()``, ``tokio::time::interval(``, ``tokio::time::sleep(``
* Python: ``time.monotonic(``, ``asyncio.sleep(`` (``asyncio.sleep(0)``, a
  yield, is fine)

unless an ``allow`` line names the file and the pattern with the reason it
must stay on real time (reconnect backoff, link liveness, log rate limits).

File format, one entry per line, ``#`` comments::

    seam  <path>
    allow <path> <pattern> -- <reason>

Usage::

    python3 scripts/check-clock-seam.py [--root DIR] [--list FILE]
    python3 scripts/check-clock-seam.py --self-test
"""

from __future__ import annotations

import argparse
import re
import sys
import tempfile
from pathlib import Path

RUST = ["Instant::now()", "tokio::time::interval(", "tokio::time::sleep("]
PYTHON = ["time.monotonic(", "asyncio.sleep("]
YIELD = re.compile(r"asyncio\.sleep\(\s*0\s*\)")


def parse(text: str) -> tuple[list[str], set[tuple[str, str]]]:
    seams: list[str] = []
    allow: set[tuple[str, str]] = set()
    for n, raw in enumerate(text.splitlines(), 1):
        line = raw.split("#", 1)[0].strip()
        if not line:
            continue
        kind, _, rest = line.partition(" ")
        rest = rest.strip()
        if kind == "seam" and rest:
            seams.append(rest)
        elif kind == "allow" and " -- " in rest:
            head, _, reason = rest.partition(" -- ")
            path, _, pattern = head.strip().partition(" ")
            if not (path and pattern.strip() and reason.strip()):
                raise SystemExit(f"clock-seam list line {n}: allow needs path, pattern and reason")
            allow.add((path, pattern.strip()))
        else:
            raise SystemExit(f"clock-seam list line {n}: expected 'seam <path>' or 'allow <path> <pattern> -- <reason>'")
    return seams, allow


def violations(root: Path, seams: list[str], allow: set[tuple[str, str]]) -> list[str]:
    out: list[str] = []
    for rel in seams:
        path = root / rel
        if not path.is_file():
            out.append(f"{rel}: listed as a seam but missing")
            continue
        patterns = RUST if path.suffix == ".rs" else PYTHON
        for n, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
            code = line.split("//", 1)[0] if path.suffix == ".rs" else line.split("#", 1)[0]
            for p in patterns:
                if p not in code or (rel, p) in allow:
                    continue
                if p == "asyncio.sleep(" and YIELD.search(code) and code.count(p) == 1:
                    continue
                out.append(f"{rel}:{n}: {p} in a file on ADOS time")
    return out


def self_test() -> int:
    with tempfile.TemporaryDirectory() as d:
        root = Path(d)
        (root / "a.rs").write_text("fn f() { let t = Instant::now(); }\n// Instant::now() in a comment\n")
        (root / "b.py").write_text("await asyncio.sleep(0.1)\nawait asyncio.sleep(0)\n")
        (root / "c.rs").write_text("let mut i = tokio::time::interval(p);\n")
        seams, allow = parse("seam a.rs\nseam b.py\nseam c.rs\nallow c.rs tokio::time::interval( -- test\n")
        got = violations(root, seams, allow)
        want = ["a.rs:1: Instant::now() in a file on ADOS time", "b.py:1: asyncio.sleep( in a file on ADOS time"]
        if got != want:
            print(f"self-test FAILED: {got!r} != {want!r}")
            return 1
    print("self-test ok")
    return 0


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", default=Path(__file__).resolve().parent.parent, type=Path)
    ap.add_argument("--list", type=Path)
    ap.add_argument("--self-test", action="store_true")
    a = ap.parse_args()
    if a.self_test:
        return self_test()
    lst = a.list or a.root / "scripts" / "clock-seam-allow.txt"
    seams, allow = parse(lst.read_text(encoding="utf-8"))
    bad = violations(a.root, seams, allow)
    for b in bad:
        print(b)
    if bad:
        print(f"{len(bad)} wall-clock use(s) in files on ADOS time; move them onto the clock or allow them with a reason in {lst}")
        return 1
    print(f"clock seam ok: {len(seams)} file(s) on ADOS time")
    return 0


if __name__ == "__main__":
    sys.exit(main())
