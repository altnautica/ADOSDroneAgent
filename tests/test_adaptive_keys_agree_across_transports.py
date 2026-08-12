# SPDX-License-Identifier: GPL-3.0-only
# Copyright (C) 2026 Altnautica — ADOS Drone Agent
"""The `adaptive` key list must be identical in both transports.

`GET /api/video/config` is served natively and by the residual Python, and both
build the `adaptive` block by selecting a fixed set of keys out of
`wfb-stats.json`. The two lists are hand-written in different languages, so
nothing makes them agree by construction.

The api-conformance harness does diff the two responses, but only field by
field against a LIVE agent — so a key added to one side and not the other shows
up as a diff on a rig, not in CI, and only if that key happens to be present in
the sidecar at that moment. This closes the gap statically: the constants
themselves are compared, so the two cannot drift in a pull request.

Parsed from the Rust source rather than imported, because there is no build step
here that would expose it. That makes the parse itself a failure mode, so it is
asserted against a floor before the comparison runs.
"""

from __future__ import annotations

import re
from pathlib import Path

import pytest

from ados.api.routes.video.encoder_config import _ADAPTIVE_KEYS

REPO_ROOT = Path(__file__).resolve().parents[1]
NATIVE = REPO_ROOT / "crates" / "ados-control" / "src" / "routes" / "video.rs"


def _native_adaptive_keys() -> list[str]:
    """The `ADAPTIVE_KEYS` entries the native route selects, in order."""
    src = NATIVE.read_text()
    marker = "const ADAPTIVE_KEYS: &[&str] = &["
    if marker not in src:
        raise AssertionError(
            f"{NATIVE.name} no longer defines `{marker}`; update this parser to "
            "match, or it will stop comparing anything"
        )
    body = src.split(marker, 1)[1].split("];", 1)[0]
    return re.findall(r'"([^"]+)"', body)


@pytest.mark.skipif(not NATIVE.exists(), reason="native crate not in this checkout")
def test_both_transports_select_the_same_adaptive_keys() -> None:
    native = _native_adaptive_keys()

    # Guard the guard: a parse that matched nothing would make the comparison
    # below pass vacuously against an empty list.
    assert len(native) >= 5, (
        f"parsed only {len(native)} keys from {NATIVE.name}; the parse is broken, "
        "so this comparison would prove nothing"
    )

    assert native == list(_ADAPTIVE_KEYS), (
        "the two transports select different keys for the `adaptive` block, so "
        "one of them serves a field the other omits:\n"
        f"  native: {native}\n"
        f"  python: {list(_ADAPTIVE_KEYS)}\n"
        f"  only native: {sorted(set(native) - set(_ADAPTIVE_KEYS))}\n"
        f"  only python: {sorted(set(_ADAPTIVE_KEYS) - set(native))}"
    )
