"""profile.conf is co-owned with the installer; detection never clobbers it."""

from __future__ import annotations

import yaml

from ados.bootstrap import profile_detect
from ados.bootstrap.profile_detect import write_profile_conf


def test_a_detected_profile_keeps_the_installer_channel_and_version(tmp_path):
    conf = tmp_path / "profile.conf"
    conf.write_text("channel: stable\nversion: 0.101.0\n", encoding="utf-8")

    assert write_profile_conf("ground_station", path=str(conf)) is True

    assert yaml.safe_load(conf.read_text()) == {
        "profile": "ground_station",
        "channel": "stable",
        "version": "0.101.0",
    }


def test_a_recorded_profile_is_never_replaced_by_detection(tmp_path):
    conf = tmp_path / "profile.conf"
    body = "profile: drone\nchannel: stable\nversion: 0.101.0\n"
    conf.write_text(body, encoding="utf-8")

    assert write_profile_conf("ground_station", path=str(conf)) is False

    assert conf.read_text() == body


def test_the_wizard_status_read_never_writes_profile_conf(monkeypatch):
    from ados.core.config import ADOSConfig
    from ados.setup.profile import build_profile_suggestion

    writes: list[object] = []
    monkeypatch.setattr(profile_detect, "write_profile_conf", lambda *a, **k: writes.append(a))
    monkeypatch.setattr(
        profile_detect,
        "detect_profile",
        lambda config_override=None: {"profile": "ground_station", "source": "detected"},
    )
    config = ADOSConfig()
    config.agent.profile = "workstation"

    build_profile_suggestion(config)

    assert writes == []
