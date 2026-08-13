# SPDX-License-Identifier: GPL-3.0-only
# Copyright (C) 2026 Altnautica — ADOS Drone Agent
"""The WFB status derivation and the radio block must exist exactly once.

Three copies of this derivation shipped at once: `/api/wfb` and `/api/status/full`
each carried a private `derive_wfb_status` + `build_status_from_stats_file` + base
block + finalize legs, and the cloud heartbeat carried none at all and served an
`absent` radio block forever because writing a fourth was the only way to get one.

A same-name fork is the acute form of the reader/producer defect, and it has
already cost a shipped regression in this repo: two same-named SEI parsers, one
of which had a fix the other lacked, and the consolidation kept the wrong one after
comparing exported names rather than behaviour.

Nothing else catches this. Both copies compile, both are reachable, both have
passing tests, and the drift only shows as two transports disagreeing about the
same radio on the same rig.
"""

from __future__ import annotations

import re
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
CRATES = REPO_ROOT / "crates"

#: The one module allowed to define them.
CANONICAL = "ados-protocol/src/wfb_status.rs"

#: The derivation's load-bearing functions. Each must have exactly one definition
#: across the whole workspace.
SINGLE_SOURCED = (
    "derive_wfb_status",
    "build_status_from_stats_file_at",
    "wfb_base_block",
    "finalize_wfb_status",
    "build_radio_block",
    "radio_absent_block",
    "regulatory_domain",
    "read_regulatory_domain",
    "get_channel",
)

#: A parse that finds fewer files than this is broken, not proof of a clean tree.
MIN_RUST_FILES = 100


def _rust_sources() -> list[Path]:
    """Every crate source file, excluding build artifacts under `target/`."""
    return [
        p
        for p in CRATES.rglob("*.rs")
        if "target" not in p.relative_to(CRATES).parts
    ]


def test_the_wfb_derivation_has_exactly_one_definition_each() -> None:
    files = _rust_sources()
    assert len(files) >= MIN_RUST_FILES, (
        f"scanned only {len(files)} crate sources; the walk is broken, so its "
        "single-definition claim means nothing"
    )

    definitions: dict[str, list[str]] = {name: [] for name in SINGLE_SOURCED}
    for path in files:
        try:
            text = path.read_text()
        except OSError:
            continue
        rel = str(path.relative_to(CRATES))
        for name in SINGLE_SOURCED:
            # `fn <name>(` at any visibility, ignoring calls and doc mentions.
            if re.search(rf"^\s*(?:pub(?:\([^)]*\))?\s+)?fn {name}\s*[(<]", text, re.M):
                definitions[name].append(rel)

    problems: list[str] = []
    for name, where in definitions.items():
        if len(where) != 1:
            problems.append(f"  {name}: {len(where)} definitions -> {where}")
        elif where[0] != CANONICAL:
            problems.append(f"  {name}: defined in {where[0]}, expected {CANONICAL}")

    assert not problems, (
        "the WFB derivation must be single-sourced in "
        f"{CANONICAL}:\n" + "\n".join(problems) + "\n"
        "A second copy is free to drift from the one every other transport reads. "
        "Call the shared function, or move the canonical one and update this test."
    )


def test_no_crate_carries_a_second_standard_channel_table() -> None:
    """The channel -> frequency table must exist once.

    The radio block used to carry a narrower private copy that omitted channels 40
    and 44, so a link on either rendered a null `freqMhz` in the GCS while the
    status body beside it reported the right frequency. Two tables of the same
    thing is how that happens.
    """
    files = _rust_sources()
    assert len(files) >= MIN_RUST_FILES

    carriers = [
        str(p.relative_to(CRATES))
        for p in files
        if re.search(r"^\s*(?:pub\s+)?const STANDARD_CHANNELS\s*:", p.read_text(), re.M)
    ]
    assert carriers == [CANONICAL], (
        f"the standard-channel table must live only in {CANONICAL}; found {carriers}"
    )
