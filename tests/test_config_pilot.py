"""The ``pilot:`` model: the place class and the open obstacle margin."""

from __future__ import annotations

from pathlib import Path

import pytest
import yaml
from pydantic import ValidationError

from ados.core.config.pilot import PilotConfig


def test_defaults_are_an_open_place_without_a_tip_radius() -> None:
    p = PilotConfig()
    assert p.place_class == "open"
    assert p.prop_tip_radius_m is None
    assert p.obstacle_margin_m == 1.5


def test_the_open_margin_keeps_half_a_metre_in_every_class() -> None:
    PilotConfig(obstacle_margin_m=0.5)
    for place in ("open", "confined"):
        with pytest.raises(ValidationError):
            PilotConfig(place_class=place, prop_tip_radius_m=0.377, obstacle_margin_m=0.48)


def test_a_confined_place_needs_no_margin_edit() -> None:
    # The pilot flies the tip radius plus 0.1 m in a confined place, so the
    # configured (open) margin stays what it was, here the default.
    p = PilotConfig(place_class="confined", prop_tip_radius_m=0.377)
    assert p.obstacle_margin_m == 1.5
    # A large airframe is fine too: nothing ties the two together.
    PilotConfig(place_class="confined", prop_tip_radius_m=0.6, obstacle_margin_m=0.5)


def test_switching_the_class_back_and_forth_always_loads(tmp_path: Path) -> None:
    # The pilot's place_class action persists only `pilot.place_class`; every
    # file it leaves behind must load.
    path = tmp_path / "config.yaml"
    path.write_text(
        "pilot:\n  place_class: open\n  prop_tip_radius_m: 0.377\n  obstacle_margin_m: 0.5\n"
    )
    for place in ("confined", "open", "confined", "open"):
        doc = yaml.safe_load(path.read_text())
        doc["pilot"]["place_class"] = place
        path.write_text(yaml.safe_dump(doc))
        p = PilotConfig(**yaml.safe_load(path.read_text())["pilot"])
        assert p.place_class == place
        assert p.obstacle_margin_m == 0.5


def test_the_schema_bounds() -> None:
    with pytest.raises(ValidationError):
        PilotConfig(obstacle_margin_m=0.25)
    with pytest.raises(ValidationError):
        PilotConfig(place_class="tunnel")
    with pytest.raises(ValidationError):
        PilotConfig(prop_tip_radius_m=0.0)


def test_the_rc_declaration_defaults_present_and_loads_absent() -> None:
    assert PilotConfig().rc_receiver == "present"
    assert PilotConfig(rc_receiver="absent").rc_receiver == "absent"
    with pytest.raises(ValidationError):
        PilotConfig(rc_receiver="none")
