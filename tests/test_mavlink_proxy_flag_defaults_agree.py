# SPDX-License-Identifier: GPL-3.0-only
# Copyright (C) 2026 Altnautica — ADOS Drone Agent
"""The MAVLink auth-flag defaults must be identical in Python and in Rust.

The router (`crates/ados-mavlink-router`) is what actually reads these keys, but
the Pydantic model is what persists them: saving the config dumps EVERY field,
defaults included, so whatever this model declares becomes an explicit value in
the node's `config.yaml` — and an explicit value beats the router's own default.

That makes a disagreement worse than a documentation error. Declaring the
opposite of the router does not merely differ on paper: the next config write
silently pins every node in the fleet to the Python side. A key the model does
not declare at all is stripped from the file on that same write, so an operator
who set it watches their setting disappear.

So the two sides are compared here, statically. Parsed out of the Rust source
rather than imported, because there is no build step in this suite that would
expose it; that makes the parse itself a failure mode, so it is asserted before
the comparison runs.
"""

from __future__ import annotations

import re
from pathlib import Path

import pytest

from ados.core.config.mavlink import MavlinkConfig

REPO_ROOT = Path(__file__).resolve().parents[1]
ROUTER_CONFIG = (
    REPO_ROOT / "crates" / "ados-mavlink-router" / "src" / "config.rs"
)

#: Every `mavlink:` boolean whose value the router reads and the model persists.
#: Keyed by config field, valued by the Rust `fn default_<field>() -> bool`.
GATE_FIELDS = (
    "ws_proxy_enforce_auth",
    "raw_proxy_enforce_auth",
    "aux_uplink_enforce_origin",
)


def _rust_bool_defaults() -> dict[str, bool]:
    """The literal each `fn default_<field>() -> bool` returns."""
    src = ROUTER_CONFIG.read_text(encoding="utf-8")
    found: dict[str, bool] = {}
    for match in re.finditer(
        r"fn default_(\w+)\(\)\s*->\s*bool\s*\{\s*(true|false)\s*\}", src
    ):
        found[match.group(1)] = match.group(2) == "true"
    return found


@pytest.mark.skipif(
    not ROUTER_CONFIG.exists(), reason="native crate not in this checkout"
)
def test_every_gate_flag_default_matches_the_router() -> None:
    rust = _rust_bool_defaults()

    # Guard the guard: a parse that matched nothing would make the loop below
    # pass vacuously.
    assert len(rust) >= len(GATE_FIELDS), (
        f"parsed only {sorted(rust)} from {ROUTER_CONFIG.name}; the parse is "
        "broken, so this comparison would prove nothing"
    )

    model = MavlinkConfig()
    for field in GATE_FIELDS:
        assert field in rust, (
            f"the router no longer declares `default_{field}`; either it stopped "
            f"reading `mavlink.{field}` (delete it from the model too) or this "
            "list is stale"
        )
        assert getattr(model, field) == rust[field], (
            f"mavlink.{field} defaults to {getattr(model, field)!r} in the model "
            f"and {rust[field]!r} in the router. Persisting the config would pin "
            "every node to the model's value, so the router's default would "
            "never take effect again."
        )


@pytest.mark.skipif(
    not ROUTER_CONFIG.exists(), reason="native crate not in this checkout"
)
def test_the_two_raw_edges_are_not_enforced_by_default() -> None:
    """Pinned, because turning either on by default is a breaking change.

    The raw TCP/UDP edges carry no credential channel, and the aux uplink is
    taken only on a ground station relaying a drone that may be airborne. A
    default flip on either refuses a working third-party ground station with no
    remedy available on the client side.
    """
    model = MavlinkConfig()
    assert model.raw_proxy_enforce_auth is False
    assert model.aux_uplink_enforce_origin is False
    # The WebSocket is the deliberate exception: it has two credential channels.
    assert model.ws_proxy_enforce_auth is True


def test_the_flags_survive_a_round_trip_through_the_model() -> None:
    """An operator who sets one must still have it after the next config write.

    This is the failure a Rust-only key has: the model dumps every declared
    field and drops everything else, so an undeclared key vanishes from
    `config.yaml` the first time the agent persists it.
    """
    dumped = MavlinkConfig(
        raw_proxy_enforce_auth=True, aux_uplink_enforce_origin=True
    ).model_dump()
    assert dumped["raw_proxy_enforce_auth"] is True
    assert dumped["aux_uplink_enforce_origin"] is True
    reloaded = MavlinkConfig(**dumped)
    assert reloaded.raw_proxy_enforce_auth is True
    assert reloaded.aux_uplink_enforce_origin is True
