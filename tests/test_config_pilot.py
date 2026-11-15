"""The ``pilot:`` model: the place class and the obstacle margin floor."""

from __future__ import annotations

import pytest
from pydantic import ValidationError

from ados.core.config.pilot import PilotConfig


def test_defaults_are_an_open_place_without_a_tip_radius() -> None:
    p = PilotConfig()
    assert p.place_class == "open"
    assert p.prop_tip_radius_m is None
    assert p.obstacle_margin_m == 1.5


def test_an_open_place_keeps_half_a_metre() -> None:
    PilotConfig(obstacle_margin_m=0.5)
    with pytest.raises(ValidationError, match="at least 0.500"):
        PilotConfig(obstacle_margin_m=0.45)


def test_a_confined_place_keeps_the_tip_radius_plus_a_tenth() -> None:
    p = PilotConfig(place_class="confined", prop_tip_radius_m=0.377, obstacle_margin_m=0.477)
    assert p.obstacle_margin_m == 0.477
    with pytest.raises(ValidationError, match="prop_tip_radius_m"):
        PilotConfig(place_class="confined", prop_tip_radius_m=0.377, obstacle_margin_m=0.45)
    # A large airframe needs more than the open floor.
    with pytest.raises(ValidationError, match="at least 0.700"):
        PilotConfig(place_class="confined", prop_tip_radius_m=0.6, obstacle_margin_m=0.6)


def test_a_confined_place_without_a_radius_keeps_the_open_floor() -> None:
    PilotConfig(place_class="confined", obstacle_margin_m=0.5)
    with pytest.raises(ValidationError):
        PilotConfig(place_class="confined", obstacle_margin_m=0.4)


def test_the_schema_bounds() -> None:
    with pytest.raises(ValidationError):
        PilotConfig(obstacle_margin_m=0.25)
    with pytest.raises(ValidationError):
        PilotConfig(place_class="tunnel")
    with pytest.raises(ValidationError):
        PilotConfig(prop_tip_radius_m=0.0)
