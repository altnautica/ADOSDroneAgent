"""The mesh carrier binds the adapter the native resolver assigned to the mesh.

The native relay / receiver loop resolves the WFB adapters, leaves one to the
mesh (role ``mesh``) and publishes it as ``mesh_adapter`` on its role sidecar.
The mesh manager binds that adapter, keeps an operator interface override
above it, and falls back to its own driver-keyed pick only when no adapter is
published.
"""

from __future__ import annotations

import json
import os
import time
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

import pytest

from ados.services.ground_station import _mesh_identity as mi
from ados.services.ground_station import mesh_manager as mm
from ados.services.network import interface_roles


def _write_sidecar(run_dir: Path, name: str, body: dict, age_s: float = 0.0) -> None:
    path = run_dir / name
    path.write_text(json.dumps(body))
    if age_s:
        stamp = time.time() - age_s
        os.utime(path, (stamp, stamp))


def test_a_fresh_role_sidecar_names_the_mesh_adapter(tmp_path: Path) -> None:
    _write_sidecar(tmp_path, "wfb-relay.json", {"role": "relay", "mesh_adapter": "wlan3"})
    _write_sidecar(tmp_path, "wfb-receiver.json", {"role": "receiver", "mesh_adapter": "wlan4"})
    assert mi._published_mesh_adapter("relay", tmp_path) == "wlan3"
    assert mi._published_mesh_adapter("receiver", tmp_path) == "wlan4"


def test_no_usable_publication_reads_as_none(tmp_path: Path) -> None:
    # Absent sidecar.
    assert mi._published_mesh_adapter("relay", tmp_path) is None
    # No adapter has the mesh role.
    _write_sidecar(tmp_path, "wfb-relay.json", {"role": "relay", "mesh_adapter": None})
    assert mi._published_mesh_adapter("relay", tmp_path) is None
    # A sidecar written before the field existed.
    _write_sidecar(tmp_path, "wfb-relay.json", {"role": "relay"})
    assert mi._published_mesh_adapter("relay", tmp_path) is None
    # A stopped writer: older than the registry window.
    _write_sidecar(
        tmp_path, "wfb-relay.json", {"mesh_adapter": "wlan3"}, age_s=mi._ROLE_SIDECAR_MAX_AGE_S + 5
    )
    assert mi._published_mesh_adapter("relay", tmp_path) is None
    # Unparseable.
    (tmp_path / "wfb-relay.json").write_text("{not json")
    assert mi._published_mesh_adapter("relay", tmp_path) is None
    # The direct role runs no mesh.
    assert mi._published_mesh_adapter("direct", tmp_path) is None


def test_pick_prefers_override_then_published_then_driver_roles(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    monkeypatch.setattr(mi, "ADOS_RUN_DIR", tmp_path)
    monkeypatch.setattr(
        interface_roles,
        "assign_roles",
        lambda: SimpleNamespace(mesh="wlan9", mgmt_wifi="wlan0"),
    )
    # Nothing published: the driver-keyed pick.
    assert mi._pick_mesh_iface(None, "relay") == "wlan9"
    # Published: the resolver-assigned adapter wins over the driver-keyed pick.
    _write_sidecar(tmp_path, "wfb-relay.json", {"mesh_adapter": "wlan3"})
    assert mi._pick_mesh_iface(None, "relay") == "wlan3"
    # The operator override wins over both.
    assert mi._pick_mesh_iface("wlan7", "relay") == "wlan7"


class _Proc:
    def __init__(self, rc: int | None) -> None:
        self.returncode = rc
        self.pid = 1
        self.terminated = False

    def poll(self) -> int | None:
        return self.returncode

    def terminate(self) -> None:
        self.terminated = True
        self.returncode = 0

    def wait(self, timeout: float | None = None) -> int | None:
        return self.returncode


def _manager(override: str | None) -> mm.MeshManager:
    config = SimpleNamespace(
        ground_station=SimpleNamespace(
            cloud_uplink="auto",
            mesh=SimpleNamespace(
                bat_iface="bat0",
                carrier="802.11s",
                channel=1,
                interface_override=override,
            ),
        )
    )
    with patch.object(mm, "get_current_role", return_value="receiver"):
        return mm.MeshManager(config)  # type: ignore[arg-type]


def _wire(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> tuple[list, list, list]:
    sys_net = tmp_path / "net"
    for iface in ("wlan2", "wlan5"):
        (sys_net / iface).mkdir(parents=True)
    run_dir = tmp_path / "run"
    run_dir.mkdir()
    monkeypatch.setattr(mi, "ADOS_RUN_DIR", run_dir)
    monkeypatch.setattr(mm, "_SYS_CLASS_NET", sys_net)
    monkeypatch.setattr(mm, "UPLINK_ACTIVE_FLAG", tmp_path / "uplink-active")
    commands: list[list[str]] = []
    monkeypatch.setattr(mm, "_run", lambda cmd, timeout=10.0: commands.append(cmd) or (0, "", ""))
    joins: list[str] = []
    monkeypatch.setattr(
        mm, "_bring_up_mesh_iface", lambda iface, *_a: joins.append(iface) or _Proc(None)
    )
    binds: list[tuple[str, str]] = []
    monkeypatch.setattr(mm, "_bind_iface_to_bat", lambda i, b: binds.append((i, b)) or True)
    monkeypatch.setattr(mm, "_iface_joined", lambda *_a: True)
    monkeypatch.setattr(mm, "_bat_member", lambda *_a: True)
    _write_sidecar(run_dir, "wfb-receiver.json", {"mesh_adapter": "wlan2"})
    return commands, joins, binds


def test_tick_moves_the_mesh_to_the_resolver_assigned_adapter(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    commands, joins, binds = _wire(monkeypatch, tmp_path)
    # The mesh came up on the fallback pick before the receiver published.
    manager = _manager(None)
    manager._mesh_iface = "wlan5"
    manager._mesh_id = "ados-abc"
    old_join = _Proc(None)
    manager._wpa = old_join

    manager.tick()

    # The old join is stopped and the old adapter leaves batman first.
    assert old_join.terminated
    assert ["batctl", "if", "del", "wlan5"] in commands
    assert joins == ["wlan2"]
    assert binds == [("wlan2", "bat0")]
    assert manager._mesh_iface == "wlan2"

    # Settled on the assigned adapter: the next pass changes nothing.
    joins.clear()
    manager.tick()
    assert joins == []


def test_an_operator_override_is_never_reassigned(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    _commands, joins, _binds = _wire(monkeypatch, tmp_path)
    manager = _manager("wlan5")
    manager._mesh_iface = "wlan5"
    manager._mesh_id = "ados-abc"
    manager._wpa = _Proc(None)

    manager.tick()

    assert joins == []
    assert manager._mesh_iface == "wlan5"
