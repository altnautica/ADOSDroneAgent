"""The profile list agrees across the contract, Python, Rust and TypeScript."""

from __future__ import annotations

import ast
import re
import tomllib
import typing
from pathlib import Path

import pytest

from ados.core._contracts_generated import PROFILES
from ados.core.config.agent import AgentProfile
from ados.core.profile import normalize_profile

_REPO = Path(__file__).resolve().parents[1]
_CONTRACT = _REPO / "crates" / "ados-protocol" / "contracts.toml"
_RUST = _REPO / "crates" / "ados-protocol" / "src" / "contracts.rs"
_TS = _REPO.parent / "ADOSMissionControl" / "src" / "lib" / "plugins" / "contracts.generated.ts"


def _contract_profiles() -> list[str]:
    with _CONTRACT.open("rb") as fh:
        return list(tomllib.load(fh)["profiles"]["values"])


def _list_literal(text: str, pattern: str) -> list[str]:
    match = re.search(pattern, text)
    assert match, f"pattern not found: {pattern}"
    return list(ast.literal_eval(match.group(1)))


def test_python_matches_contract() -> None:
    assert list(PROFILES) == _contract_profiles()


def test_rust_matches_contract() -> None:
    text = _RUST.read_text(encoding="utf-8")
    values = _list_literal(text, r"pub const PROFILES: &\[&str\] = &(\[[^\]]*\]);")
    assert values == _contract_profiles()


def test_typescript_matches_contract() -> None:
    if not _TS.exists():
        pytest.skip("Mission Control checkout not present")
    text = _TS.read_text(encoding="utf-8")
    values = _list_literal(text, r"export const PROFILES = (\[[^\]]*\]) as const;")
    assert values == _contract_profiles()


def test_agent_profile_literal_covers_contract() -> None:
    assert set(PROFILES) <= set(typing.get_args(AgentProfile))


@pytest.mark.parametrize("value", PROFILES)
def test_normalize_profile_keeps_each_profile(value: str) -> None:
    # Wire form is the hyphen spelling; single-word profiles are unchanged.
    assert normalize_profile(value) == value.replace("_", "-")


# Hand-written TypeScript profile unions in the agent's own web bundles. Each
# lists exactly the contract profiles, plus the `unknown` sentinel where the
# union also models an unreported profile.
_TS_UNIONS = [
    ("web-shared/src/use-profile.ts", "AgentProfile", {"unknown"}),
    ("cockpit/src/shared/use-profile.ts", "AgentProfile", {"unknown"}),
    ("dashboard/src/shared/use-profile.ts", "AgentProfile", {"unknown"}),
    ("dashboard/src/lib/types.ts", "Profile", {"auto", "unknown"}),
    ("dashboard/src/lib/node-identity.ts", "AllowedProfile", None),
]


def _ts_union(text: str, name: str) -> set[str]:
    match = re.search(rf"export type {name}\s*=\s*([^;]+);", text)
    assert match, f"type {name} not found"
    return set(re.findall(r'"([^"]+)"', match.group(1)))


@pytest.mark.parametrize(("rel", "name", "extra"), _TS_UNIONS)
def test_hand_written_ts_unions_match_contract(rel: str, name: str, extra: set[str] | None) -> None:
    members = _ts_union((_REPO / rel).read_text(encoding="utf-8"), name)
    assert members - (extra or set()) == set(_contract_profiles()), rel
