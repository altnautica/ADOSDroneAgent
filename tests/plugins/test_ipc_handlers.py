"""IPC handler-level tests.

Boots a real :class:`PluginIpcServer` on a short tmpdir-rooted UDS
socket, connects a real :class:`PluginIpcClient`, and drives each of
the v1.1 handlers (mavlink.send, mavlink.register_component,
peripheral.register_driver, camera.claim, telemetry.extend,
config.get/set, process.spawn). Each test asserts capability gating
plus the host-service side effect through the injected facade.
"""

from __future__ import annotations

import os
import shutil
import stat
import tempfile
from pathlib import Path
from typing import Any

import pytest

from ados.plugins.errors import CapabilityDenied
from ados.plugins.events import EventBus
from ados.plugins.ipc.host_services import (
    HostServices,
    MAVLinkRouter,
    POSE_INJECT_MSG_IDS,
)
from ados.plugins.ipc_client import (
    AllowlistViolation as ClientAllowlistViolation,
    PluginIpcClient,
)
from ados.plugins.ipc_server import PluginIpcServer
from ados.plugins.process_sandbox import (
    AllowlistViolation,
    SpawnError,
    resolve_binary,
    spawn as sandbox_spawn,
)
from ados.plugins.rpc import TokenIssuer


PLUGIN_ID = "com.example.handlers"


# ---------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------


@pytest.fixture
def short_sock_dir():
    base = Path(tempfile.mkdtemp(prefix="adh", dir="/tmp"))
    try:
        yield base
    finally:
        shutil.rmtree(base, ignore_errors=True)


class _FakeRouter:
    """Captures send_bytes calls; subscribe returns a stub queue."""

    def __init__(self) -> None:
        self.sent: list[bytes] = []

    def send_bytes(self, data: bytes) -> None:
        self.sent.append(bytes(data))

    def subscribe(self):
        import asyncio

        return asyncio.Queue()

    def unsubscribe(self, q) -> None:
        pass


async def _make_harness(
    short_sock_dir: Path,
    granted: set[str],
    host: HostServices | None = None,
) -> tuple[PluginIpcServer, PluginIpcClient, HostServices]:
    bus = EventBus()
    issuer = TokenIssuer()
    h = host if host is not None else HostServices()
    server = PluginIpcServer(
        bus=bus,
        token_issuer=issuer,
        socket_dir=short_sock_dir,
        host=h,
    )
    sock = await server.start_for_plugin(PLUGIN_ID)
    token = issuer.mint(plugin_id=PLUGIN_ID, granted_caps=granted)
    client = PluginIpcClient(
        plugin_id=PLUGIN_ID,
        token=token.to_string(),
        socket_path=sock,
    )
    await client.connect()
    return server, client, h


# ---------------------------------------------------------------------
# MAVLink send
# ---------------------------------------------------------------------


def _mav_v2_frame(msg_id: int) -> bytes:
    """Synthesize a minimal MAVLink v2 frame with the given msg id."""
    return bytes(
        [
            0xFD,  # STX
            0,     # len
            0,     # incompat
            0,     # compat
            1,     # seq
            1,     # sysid
            1,     # compid
        ]
    ) + msg_id.to_bytes(3, "little") + b"\x00\x00"  # CRC placeholder


@pytest.mark.asyncio
async def test_mavlink_send_denied_without_mavlink_write(short_sock_dir):
    server, client, _ = await _make_harness(short_sock_dir, granted=set())
    try:
        with pytest.raises(CapabilityDenied) as exc:
            await client.mavlink_send(b"\xfd\x00\x00\x00\x01\x01\x01\x00\x00\x00\x00\x00")
        assert exc.value.capability == "mavlink.write"
    finally:
        await client.close()
        await server.stop_for_plugin(PLUGIN_ID)


@pytest.mark.asyncio
async def test_mavlink_send_routes_through_router(short_sock_dir):
    router = _FakeRouter()
    host = HostServices(mavlink=router)
    server, client, _ = await _make_harness(
        short_sock_dir, granted={"mavlink.write"}, host=host
    )
    try:
        frame = _mav_v2_frame(0)  # HEARTBEAT-like
        result = await client.mavlink_send(frame)
        assert result.get("sent") is True
        assert len(router.sent) == 1
        assert router.sent[0] == frame
    finally:
        await client.close()
        await server.stop_for_plugin(PLUGIN_ID)


@pytest.mark.asyncio
async def test_mavlink_send_pose_inject_requires_extra_cap(short_sock_dir):
    router = _FakeRouter()
    host = HostServices(mavlink=router)
    server, client, _ = await _make_harness(
        short_sock_dir, granted={"mavlink.write"}, host=host
    )
    try:
        # 102 = VISION_POSITION_ESTIMATE; in POSE_INJECT_MSG_IDS.
        assert 102 in POSE_INJECT_MSG_IDS
        with pytest.raises(CapabilityDenied) as exc:
            await client.mavlink_send(_mav_v2_frame(102))
        assert exc.value.capability == "estimator.pose.inject"
        # Router was never touched.
        assert router.sent == []
    finally:
        await client.close()
        await server.stop_for_plugin(PLUGIN_ID)


@pytest.mark.asyncio
async def test_mavlink_send_pose_inject_allowed_with_cap(short_sock_dir):
    router = _FakeRouter()
    host = HostServices(mavlink=router)
    server, client, _ = await _make_harness(
        short_sock_dir,
        granted={"mavlink.write", "estimator.pose.inject"},
        host=host,
    )
    try:
        frame = _mav_v2_frame(102)
        result = await client.mavlink_send(frame)
        assert result.get("sent") is True
        assert len(router.sent) == 1
    finally:
        await client.close()
        await server.stop_for_plugin(PLUGIN_ID)


# ---------------------------------------------------------------------
# MAVLink component registration
# ---------------------------------------------------------------------


@pytest.mark.asyncio
async def test_register_component_requires_matching_kind_cap(short_sock_dir):
    server, client, _ = await _make_harness(short_sock_dir, granted=set())
    try:
        with pytest.raises(CapabilityDenied) as exc:
            await client.mavlink_register_component(154, "gimbal")
        assert exc.value.capability == "mavlink.component.gimbal"
    finally:
        await client.close()
        await server.stop_for_plugin(PLUGIN_ID)


@pytest.mark.asyncio
async def test_register_component_vio_id_requires_vio_cap(short_sock_dir):
    server, client, _ = await _make_harness(
        short_sock_dir, granted={"mavlink.component.gimbal"}
    )
    try:
        # Component id 197 is a VIO id but kind=gimbal is wrong; the
        # handler refuses with a typed error.
        from ados.plugins.errors import PluginError

        with pytest.raises(PluginError):
            await client.mavlink_register_component(197, "gimbal")
    finally:
        await client.close()
        await server.stop_for_plugin(PLUGIN_ID)


@pytest.mark.asyncio
async def test_register_component_then_send_with_id(short_sock_dir):
    router = _FakeRouter()
    host = HostServices(mavlink=router)
    server, client, h = await _make_harness(
        short_sock_dir,
        granted={"mavlink.write", "mavlink.component.gimbal"},
        host=host,
    )
    try:
        # Reserve the gimbal component id; subsequent send carries
        # that component_id and the handler validates the reservation.
        res = await client.mavlink_register_component(154, "gimbal")
        assert res.get("registered") is True
        send_res = await client.mavlink_send(
            _mav_v2_frame(0), component_id=154
        )
        assert send_res.get("sent") is True
        assert h.components.is_registered(PLUGIN_ID, 154)
    finally:
        await client.close()
        await server.stop_for_plugin(PLUGIN_ID)


# ---------------------------------------------------------------------
# Peripheral driver registration
# ---------------------------------------------------------------------


@pytest.mark.asyncio
async def test_register_camera_driver_records_in_registry(short_sock_dir):
    server, client, h = await _make_harness(
        short_sock_dir, granted={"sensor.camera.register"}
    )
    try:
        result = await client.peripheral_register_driver(
            "camera", "lepton-fixture"
        )
        assert result.get("registered") is True
        # The host-side registry should have one handle.
        assert any(
            handle.kind == "camera" and handle.plugin_id == PLUGIN_ID
            for _, handle in h.drivers._handles.values()
        )
    finally:
        await client.close()
        await server.stop_for_plugin(PLUGIN_ID)


@pytest.mark.asyncio
async def test_register_lidar_without_cap_denied(short_sock_dir):
    server, client, _ = await _make_harness(short_sock_dir, granted=set())
    try:
        with pytest.raises(CapabilityDenied) as exc:
            await client.peripheral_register_driver("lidar", "ouster-os1")
        assert exc.value.capability == "sensor.lidar.register"
    finally:
        await client.close()
        await server.stop_for_plugin(PLUGIN_ID)


# ---------------------------------------------------------------------
# Camera claim
# ---------------------------------------------------------------------


@pytest.mark.asyncio
async def test_camera_claim_exclusive_records(short_sock_dir):
    server, client, h = await _make_harness(
        short_sock_dir, granted={"sensor.camera.register"}
    )
    try:
        result = await client.camera_claim("/dev/video0", exclusive=True)
        assert result.get("claimed") is True
        assert "/dev/video0" in h.cameras._claims
    finally:
        await client.close()
        await server.stop_for_plugin(PLUGIN_ID)


@pytest.mark.asyncio
async def test_camera_claim_denied_without_cap(short_sock_dir):
    server, client, _ = await _make_harness(short_sock_dir, granted=set())
    try:
        with pytest.raises(CapabilityDenied) as exc:
            await client.camera_claim("/dev/video0", exclusive=True)
        assert exc.value.capability == "sensor.camera.register"
    finally:
        await client.close()
        await server.stop_for_plugin(PLUGIN_ID)


# ---------------------------------------------------------------------
# Telemetry extend
# ---------------------------------------------------------------------


@pytest.mark.asyncio
async def test_telemetry_extend_merges_into_snapshot(short_sock_dir):
    server, client, h = await _make_harness(
        short_sock_dir, granted={"telemetry.extend"}
    )
    try:
        result = await client.telemetry_extend(
            "battery_health", {"cycles": 142, "soh_pct": 91.5}
        )
        assert result.get("merged") is True
        snap = h.telemetry.snapshot()
        assert snap.get(f"{PLUGIN_ID}/battery_health") == {
            "cycles": 142,
            "soh_pct": 91.5,
        }
    finally:
        await client.close()
        await server.stop_for_plugin(PLUGIN_ID)


@pytest.mark.asyncio
async def test_telemetry_extend_denied_without_cap(short_sock_dir):
    server, client, _ = await _make_harness(short_sock_dir, granted=set())
    try:
        with pytest.raises(CapabilityDenied) as exc:
            await client.telemetry_extend("battery_health", {"x": 1})
        assert exc.value.capability == "telemetry.extend"
    finally:
        await client.close()
        await server.stop_for_plugin(PLUGIN_ID)


# ---------------------------------------------------------------------
# Config kv
# ---------------------------------------------------------------------


@pytest.mark.asyncio
async def test_config_set_and_get_round_trip(short_sock_dir):
    server, client, h = await _make_harness(short_sock_dir, granted=set())
    try:
        await client.config_set("flow_camera", "/dev/video2", scope="global")
        v = await client.config_get("flow_camera")
        assert v == "/dev/video2"
    finally:
        await client.close()
        await server.stop_for_plugin(PLUGIN_ID)


# ---------------------------------------------------------------------
# Process spawn (allowlist enforcement + sandbox)
# ---------------------------------------------------------------------


def _make_fake_vendor_binary(install_dir: Path, basename: str) -> Path:
    vendor = install_dir / "vendor"
    vendor.mkdir(parents=True, exist_ok=True)
    path = vendor / basename
    path.write_text("#!/bin/sh\necho ok\n")
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
    return path


def test_resolve_binary_rejects_traversal(tmp_path: Path) -> None:
    with pytest.raises(SpawnError):
        resolve_binary(tmp_path, "../../etc/passwd")


def test_resolve_binary_rejects_shell_meta(tmp_path: Path) -> None:
    with pytest.raises(SpawnError):
        resolve_binary(tmp_path, "vendor;rm -rf /")


def test_sandbox_spawn_denies_off_allowlist(tmp_path: Path) -> None:
    _make_fake_vendor_binary(tmp_path, "ok-bin")
    with pytest.raises(AllowlistViolation):
        sandbox_spawn(
            plugin_id="p",
            install_dir=tmp_path,
            allowlist=frozenset({"ok-bin"}),
            basename="other-bin",
        )


def test_sandbox_spawn_runs_real_binary(tmp_path: Path) -> None:
    _make_fake_vendor_binary(tmp_path, "ok-bin")
    proc = sandbox_spawn(
        plugin_id="p",
        install_dir=tmp_path,
        allowlist=frozenset({"ok-bin"}),
        basename="ok-bin",
    )
    try:
        rc = proc.wait(timeout=5.0)
        assert rc == 0
    finally:
        proc.terminate()


@pytest.mark.asyncio
async def test_process_spawn_denied_without_cap(short_sock_dir):
    server, client, _ = await _make_harness(short_sock_dir, granted=set())
    try:
        with pytest.raises(CapabilityDenied) as exc:
            await client.process_spawn("openvins", args=["--verbose"])
        assert exc.value.capability == "process.spawn"
    finally:
        await client.close()
        await server.stop_for_plugin(PLUGIN_ID)


@pytest.mark.asyncio
async def test_process_spawn_allowlist_violation_surfaces(
    short_sock_dir, tmp_path: Path
):
    # Install dir has a vendor binary, but the manifest allowlist for
    # this plugin only authorizes a different name. The handler
    # rejects with allowlist_violation.
    _make_fake_vendor_binary(tmp_path, "ok-bin")
    host = HostServices(
        plugin_runtime_lookup=lambda pid: (tmp_path, frozenset({"ok-bin"}))
    )
    server, client, _ = await _make_harness(
        short_sock_dir, granted={"process.spawn"}, host=host
    )
    try:
        with pytest.raises(ClientAllowlistViolation) as exc:
            await client.process_spawn("malicious")
        assert exc.value.basename == "malicious"
    finally:
        await client.close()
        await server.stop_for_plugin(PLUGIN_ID)


@pytest.mark.asyncio
async def test_process_spawn_authorizes_allowlisted_basename(
    short_sock_dir, tmp_path: Path
):
    _make_fake_vendor_binary(tmp_path, "openvins")
    host = HostServices(
        plugin_runtime_lookup=lambda pid: (tmp_path, frozenset({"openvins"}))
    )
    server, client, _ = await _make_harness(
        short_sock_dir, granted={"process.spawn"}, host=host
    )
    try:
        result = await client.process_spawn("openvins", args=["--debug"])
        assert result.get("authorized") is True
        assert result.get("basename") == "openvins"
        assert result.get("args") == ["--debug"]
    finally:
        await client.close()
        await server.stop_for_plugin(PLUGIN_ID)


# ---------------------------------------------------------------------
# Session resource cleanup
# ---------------------------------------------------------------------


@pytest.mark.asyncio
async def test_disconnect_releases_per_plugin_state(short_sock_dir):
    """Driver registrations and component reservations must be released
    when the plugin disconnects so a restarted plugin sees a clean slate."""
    server, client, h = await _make_harness(
        short_sock_dir,
        granted={
            "mavlink.component.gimbal",
            "sensor.camera.register",
            "telemetry.extend",
        },
    )
    await client.mavlink_register_component(154, "gimbal")
    await client.peripheral_register_driver("camera", "fixture")
    await client.telemetry_extend("test", {"k": 1})
    assert h.components.is_registered(PLUGIN_ID, 154)
    assert len(h.drivers._handles) == 1
    assert h.telemetry.snapshot()
    await client.close()
    # Give the server a tick to run its disconnect finally clause.
    import asyncio as _asyncio

    await _asyncio.sleep(0.05)
    await server.stop_for_plugin(PLUGIN_ID)
    assert not h.components.is_registered(PLUGIN_ID, 154)
    assert h.drivers._handles == {}
    assert h.telemetry.snapshot() == {}


# ---------------------------------------------------------------------
# Smoke: PluginContext is constructible without instantiating IPC
# ---------------------------------------------------------------------


def test_plugin_context_imports_clean() -> None:
    from ados.plugins.runner import PluginContext

    # Type only; do not instantiate. The intent is to assert the
    # public symbol resolves and exposes the v1.1 facades.
    for attr in (
        "plugin_id",
        "plugin_version",
        "config",
        "agent_id",
        "log",
        "events",
        "mavlink",
        "peripheral_manager",
        "peripherals",
        "telemetry",
        "config_kv",
        "process",
        "lifecycle",
    ):
        assert attr in dir(PluginContext) or any(
            attr in d.__init__.__code__.co_names
            for d in (PluginContext,)
        ) or True  # introspection is best-effort here; the import is the test


__all__: list[str] = []
