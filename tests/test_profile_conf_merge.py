"""`ados profile set` merges the profile sentinel instead of rewriting it."""

from __future__ import annotations

from pathlib import Path

import pytest
import yaml

from ados.cli import profile as cli_profile


def test_profile_set_keeps_channel_version_and_unknown_keys(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    conf = tmp_path / "profile.conf"
    conf.write_text(
        "profile: drone\nchannel: edge\nversion: 1.2.3\noperator_note: bench rig\n",
        encoding="utf-8",
    )
    monkeypatch.setattr(cli_profile, "PROFILE_CONF", str(conf))

    cli_profile._write_profile_conf("ground_station")

    doc = yaml.safe_load(conf.read_text(encoding="utf-8"))
    assert doc == {
        "profile": "ground_station",
        "channel": "edge",
        "version": "1.2.3",
        "operator_note": "bench rig",
    }
    assert not (tmp_path / "profile.conf.tmp").exists()


def test_profile_set_creates_a_missing_file(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    conf = tmp_path / "sub" / "profile.conf"
    monkeypatch.setattr(cli_profile, "PROFILE_CONF", str(conf))

    cli_profile._write_profile_conf("drone")

    assert yaml.safe_load(conf.read_text(encoding="utf-8")) == {"profile": "drone"}


def test_invalid_yaml_is_not_clobbered(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    conf = tmp_path / "profile.conf"
    broken = "profile: drone\nchannel: [edge\n"
    conf.write_text(broken, encoding="utf-8")
    monkeypatch.setattr(cli_profile, "PROFILE_CONF", str(conf))

    with pytest.raises(RuntimeError, match="is not valid YAML; fix or remove it"):
        cli_profile._write_profile_conf("ground_station")

    assert conf.read_text(encoding="utf-8") == broken
