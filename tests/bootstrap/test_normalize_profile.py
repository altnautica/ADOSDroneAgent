"""Profile spelling bridge between the stored and wire forms."""

from __future__ import annotations

import pytest

from ados.core import profile as core_profile
from ados.core._contracts_generated import PROFILES


@pytest.mark.parametrize("value", PROFILES)
def test_every_contract_profile_round_trips(value: str) -> None:
    wire = value.replace("_", "-")
    assert core_profile.normalize_profile(value) == wire
    assert core_profile.normalize_profile(wire) == wire
    assert core_profile.canonical_profile(wire) == value


@pytest.mark.parametrize("value", [None, "", "auto", "bogus"])
def test_unknown_values_fall_back_to_drone(value) -> None:
    assert core_profile.normalize_profile(value) == "drone"
    assert core_profile.canonical_profile(value) is None


@pytest.mark.parametrize(
    ("text", "expected"),
    [
        ("profile: visor\n", "visor"),
        ("profile: ground-station\n", "ground_station"),
        ("profile=compute\n", "compute"),
        ("profile: bogus\n", None),
    ],
)
def test_profile_conf_reader_accepts_contract_profiles(
    tmp_path, monkeypatch, text: str, expected
) -> None:
    conf = tmp_path / "profile.conf"
    conf.write_text(text, encoding="utf-8")
    monkeypatch.setattr(core_profile, "PROFILE_CONF", conf)
    assert core_profile._read_profile_conf_value() == expected


def test_visor_resolves_without_ground_role(monkeypatch) -> None:
    config = type("C", (), {"agent": type("A", (), {"profile": "visor"})()})()
    assert core_profile.current_profile_and_role(config) == ("visor", None)
