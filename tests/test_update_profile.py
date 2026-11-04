"""The profile ``ados update`` pins for the installer."""

from __future__ import annotations

import pytest

import ados.cli._main_update as update_mod
import ados.core.profile as profile_mod
from ados.core.config import ADOSConfig


def _node(monkeypatch, *, config_profile: str | None, conf: str | None) -> None:
    """A node whose config names ``config_profile`` and whose profile.conf
    names ``conf`` (``None``: absent)."""

    def load() -> ADOSConfig:
        config = ADOSConfig()
        if config_profile is not None:
            config.agent.profile = config_profile
        return config

    monkeypatch.setattr(update_mod, "load_config", load)
    monkeypatch.setattr(update_mod, "_read_profile_conf_value", lambda: conf)
    monkeypatch.setattr(profile_mod, "_read_profile_conf_value", lambda: conf)
    monkeypatch.setattr(update_mod, "_running_profile", lambda: pytest.fail("no agent call"))


def test_the_config_profile_wins_over_a_stale_profile_conf(monkeypatch) -> None:
    # Regression: a profile changed through the setup API before the switch
    # kept profile.conf current left it naming the old profile, and the update
    # pinned that, rewriting agent.profile back to it.
    _node(monkeypatch, config_profile="drone", conf="ground_station")
    assert update_mod._upgrade_profile(None) == "drone"
    _node(monkeypatch, config_profile="ground_station", conf="drone")
    assert update_mod._upgrade_profile(None) == "ground_station"


def test_auto_defers_to_profile_conf_and_nothing_known_is_none(monkeypatch) -> None:
    _node(monkeypatch, config_profile="auto", conf="visor")
    assert update_mod._upgrade_profile(None) == "visor"
    _node(monkeypatch, config_profile="auto", conf=None)
    assert update_mod._upgrade_profile(None) is None
    _node(monkeypatch, config_profile="auto", conf=None)
    assert update_mod._upgrade_profile("ground_station") == "ground_station"


def test_an_unreadable_config_asks_the_running_agent_first(monkeypatch) -> None:
    def denied() -> ADOSConfig:
        raise PermissionError("config.yaml")

    monkeypatch.setattr(update_mod, "load_config", denied)
    monkeypatch.setattr(update_mod, "_read_profile_conf_value", lambda: "ground_station")
    monkeypatch.setattr(update_mod, "_running_profile", lambda: "drone")
    assert update_mod._upgrade_profile(None) == "drone"
    monkeypatch.setattr(update_mod, "_running_profile", lambda: None)
    assert update_mod._upgrade_profile(None) == "ground_station"
