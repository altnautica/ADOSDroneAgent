"""The API runtime sizes vision models for the NPU of the board detection resolved."""

from __future__ import annotations

import builtins
import json

from ados.api.runtime import StandaloneApiRuntime
from ados.core.config import ADOSConfig
from ados.hal.detect import BoardInfo


class _Log:
    def info(self, *a, **k) -> None: ...

    def warning(self, *a, **k) -> None: ...


def test_model_manager_uses_the_npu_of_the_resolved_board(tmp_path, monkeypatch):
    # A board resolved through its override or compatible token: neither its
    # name nor its model string matches a profile by name or model pattern.
    board = BoardInfo(
        name="operator-override",
        model="Vendor carrier board",
        tier=4,
        ram_mb=8192,
        cpu_cores=8,
        npu_tops=6.0,
    )
    monkeypatch.setattr("ados.hal.detect.detect_board", lambda: board)
    cfg = ADOSConfig()
    cfg.pairing.state_path = str(tmp_path / "pairing.json")
    cfg.vision.models_dir = str(tmp_path / "models")

    runtime = StandaloneApiRuntime(
        cfg, state_client=None, log=_Log(), board_json=tmp_path / "absent-board.json"
    )

    assert runtime.board_name == "operator-override"
    assert runtime.model_manager is not None
    assert runtime.model_manager._npu_tops == 6.0


def _cfg(tmp_path) -> ADOSConfig:
    cfg = ADOSConfig()
    cfg.pairing.state_path = str(tmp_path / "pairing.json")
    cfg.vision.models_dir = str(tmp_path / "models")
    return cfg


def test_board_sidecar_is_read_first(tmp_path, monkeypatch):
    def _never():
        raise AssertionError("detect_board must not run when the sidecar exists")

    monkeypatch.setattr("ados.hal.detect.detect_board", _never)
    sidecar = tmp_path / "board.json"
    sidecar.write_text(
        json.dumps({"version": 1, "name": "Rock 5C", "soc": "rk3588s", "model": "Radxa", "npu_tops": 6.0}),
        encoding="utf-8",
    )
    runtime = StandaloneApiRuntime(_cfg(tmp_path), state_client=None, log=_Log(), board_json=sidecar)
    assert runtime.board_name == "Rock 5C"
    assert runtime.board_soc == "rk3588s"
    assert runtime.board_model == "Radxa"
    assert runtime.model_manager._npu_tops == 6.0


def test_board_is_set_when_model_manager_import_fails(tmp_path, monkeypatch):
    sidecar = tmp_path / "board.json"
    sidecar.write_text(json.dumps({"name": "Pi 5", "soc": "bcm2712"}), encoding="utf-8")
    real_import = builtins.__import__

    def _fail_model_manager(name, *args, **kwargs):
        if name == "ados.services.vision.model_manager":
            raise ImportError("model manager unavailable")
        return real_import(name, *args, **kwargs)

    monkeypatch.setattr(builtins, "__import__", _fail_model_manager)
    runtime = StandaloneApiRuntime(_cfg(tmp_path), state_client=None, log=_Log(), board_json=sidecar)
    assert runtime.model_manager is None
    assert runtime.board_name == "Pi 5"
    assert runtime.board_soc == "bcm2712"


async def test_aconfig_returns_the_current_config(tmp_path):
    runtime = StandaloneApiRuntime(
        _cfg(tmp_path), state_client=None, log=_Log(), board_json=tmp_path / "none.json"
    )
    assert await runtime.aconfig() is runtime.config
