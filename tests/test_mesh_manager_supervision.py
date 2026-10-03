"""The mesh manager keeps supervising after bringup.

Every pass re-evaluates the batman gateway mode from the uplink-active flag
(so a node whose uplink comes or goes starts or stops advertising without a
restart) and heals a mesh whose secure join was lost.
"""

from __future__ import annotations

from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

import pytest

from ados.services.ground_station import mesh_manager as mm


def _manager(role: str, cloud_uplink: str = "auto") -> mm.MeshManager:
    config = SimpleNamespace(
        ground_station=SimpleNamespace(
            cloud_uplink=cloud_uplink,
            mesh=SimpleNamespace(
                bat_iface="bat0",
                carrier="802.11s",
                channel=1,
                interface_override="wlan1",
            ),
        )
    )
    with patch.object(mm, "get_current_role", return_value=role):
        return mm.MeshManager(config)  # type: ignore[arg-type]


def test_gateway_mode_follows_the_uplink_flag_and_only_applies_changes(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    flag = tmp_path / "uplink-active"
    monkeypatch.setattr(mm, "UPLINK_ACTIVE_FLAG", flag)
    commands: list[list[str]] = []

    def _fake_run(cmd: list[str], timeout: float = 10.0) -> tuple[int, str, str]:
        commands.append(cmd)
        return 0, "", ""

    monkeypatch.setattr(mm, "_run", _fake_run)
    manager = _manager("receiver")

    # No uplink: a receiver runs as a gateway client.
    assert manager._update_gateway_mode() == "client"
    assert commands == [["batctl", "gw_mode", "client"]]

    # Unchanged decision: no command at all.
    commands.clear()
    manager._update_gateway_mode()
    assert commands == []

    # Uplink comes up: advertise as a server.
    flag.touch()
    assert manager._update_gateway_mode() == "server"
    assert commands == [["batctl", "gw_mode", "server", mm._GATEWAY_BANDWIDTH_DEFAULT]]

    commands.clear()
    manager._update_gateway_mode()
    assert commands == []

    # Uplink drops: stop advertising, back to client.
    flag.unlink()
    assert manager._update_gateway_mode() == "client"
    assert commands == [["batctl", "gw_mode", "client"]]


def test_failed_gateway_apply_is_retried_on_the_next_pass(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    monkeypatch.setattr(mm, "UPLINK_ACTIVE_FLAG", tmp_path / "uplink-active")
    results = iter([(1, "", "batctl: busy"), (0, "", "")])
    commands: list[list[str]] = []

    def _fake_run(cmd: list[str], timeout: float = 10.0) -> tuple[int, str, str]:
        commands.append(cmd)
        return next(results)

    monkeypatch.setattr(mm, "_run", _fake_run)
    manager = _manager("receiver")
    assert manager._update_gateway_mode() is None
    assert manager._update_gateway_mode() == "client"
    assert len(commands) == 2


class _Proc:
    def __init__(self, rc: int | None) -> None:
        self.returncode = rc
        self.pid = 1

    def poll(self) -> int | None:
        return self.returncode


def test_tick_rejoins_when_wpa_supplicant_died(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    sys_net = tmp_path / "net"
    (sys_net / "wlan1").mkdir(parents=True)
    monkeypatch.setattr(mm, "_SYS_CLASS_NET", sys_net)
    monkeypatch.setattr(mm, "UPLINK_ACTIVE_FLAG", tmp_path / "uplink-active")
    monkeypatch.setattr(mm, "_run", lambda cmd, timeout=10.0: (0, "", ""))
    joins: list[str] = []
    fresh = _Proc(None)

    def _fake_bring_up(iface: str, *_args: object) -> _Proc:
        joins.append(iface)
        return fresh

    binds: list[tuple[str, str]] = []
    monkeypatch.setattr(mm, "_bring_up_mesh_iface", _fake_bring_up)
    monkeypatch.setattr(
        mm, "_bind_iface_to_bat", lambda i, b: binds.append((i, b)) or True
    )

    manager = _manager("relay")
    manager._mesh_iface = "wlan1"
    manager._mesh_id = "ados-abc"
    manager._wpa = _Proc(1)  # exited

    manager.tick()

    assert joins == ["wlan1"]
    assert binds == [("wlan1", "bat0")]
    assert manager._wpa is fresh
