"""The API runtime sizes vision models for the NPU of the board detection resolved."""

from __future__ import annotations

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

    runtime = StandaloneApiRuntime(cfg, state_client=None, log=_Log())

    assert runtime.board_name == "operator-override"
    assert runtime.model_manager is not None
    assert runtime.model_manager._npu_tops == 6.0
