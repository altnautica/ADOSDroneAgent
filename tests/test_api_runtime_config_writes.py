"""The residual API never writes a stale config copy back over other writers."""

from __future__ import annotations

import os

import yaml

from ados.api.runtime import ApiRuntimeFacade, StandaloneApiRuntime
from ados.core.config import load_config
from ados.core.config.writer import set_config_values
from ados.setup.service import apply_cloud_choice


class _Log:
    def info(self, *a, **k) -> None: ...

    def warning(self, *a, **k) -> None: ...


def _no_board():
    raise OSError("no board on the test host")


def _runtime(tmp_path, monkeypatch) -> tuple[ApiRuntimeFacade, os.PathLike]:
    monkeypatch.setattr("ados.hal.detect.detect_board", _no_board)
    cfg_path = tmp_path / "config.yaml"
    cfg_path.write_text(
        "video:\n  wfb:\n    channel: 149\n"
        f"pairing:\n  state_path: {tmp_path / 'pairing.json'}\n",
        encoding="utf-8",
    )
    config = load_config(cfg_path)
    raw = StandaloneApiRuntime(config, state_client=None, log=_Log(), config_path=cfg_path)
    return ApiRuntimeFacade(raw), cfg_path


def test_a_setup_write_keeps_a_change_another_writer_made_after_start(tmp_path, monkeypatch):
    app, cfg_path = _runtime(tmp_path, monkeypatch)
    assert app.config.video.wfb.channel == 149

    # A native route (or the CLI) moves the radio after this process loaded.
    assert set_config_values({"video.wfb.channel": 161}, path=cfg_path)

    result = apply_cloud_choice(app, mode="local")

    assert result.ok is True
    on_disk = yaml.safe_load(cfg_path.read_text(encoding="utf-8"))
    assert on_disk["video"]["wfb"]["channel"] == 161
    assert on_disk["server"]["mode"] == "local"


def test_config_reads_follow_the_file(tmp_path, monkeypatch):
    app, cfg_path = _runtime(tmp_path, monkeypatch)
    assert app.config.video.wfb.channel == 149

    assert set_config_values({"video.wfb.channel": 161}, path=cfg_path)

    assert app.config.video.wfb.channel == 161


def test_an_out_of_range_write_is_refused(tmp_path, monkeypatch):
    app, cfg_path = _runtime(tmp_path, monkeypatch)
    before = cfg_path.read_bytes()

    result = app.write_config({"video.wfb.fec_n": 300})

    assert result.ok is False
    assert cfg_path.read_bytes() == before
