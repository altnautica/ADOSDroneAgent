"""The capability sandbox: what a grant actually changes in the unit.

Three capabilities cannot be enforced on the wire, because the plugin does the
thing with a direct syscall inside its own process and there is no RPC the host
could refuse: ``hardware.*`` (open a device node), ``network.outbound``
(``socket(AF_INET)``), and ``filesystem.host`` (read outside its own tree). For
those the grant has to change the *sandbox*, and the only place that can
express it is the unit file systemd starts the plugin from.

These tests are the evidence behind ``enforced = true`` on those rows in
``capabilities.toml``. Before the sandbox existed all ten were decorative: the
install dialog told the operator a plugin had been granted (or denied) I2C,
SPI, GPIO, USB, host filesystem and outbound network, and nothing downstream
acted on the answer.

The other thing asserted here is **cross-language agreement**. Either
lifecycle path may be the one that wrote a given unit — this module's
supervisor owns the local REST path, ``ados-plugin-host``'s Rust supervisor
owns the cloud-relay path — so a plugin must not get a different sandbox
depending on which surface the operator used. The Rust half of the map is
asserted in ``ados-plugin-host/src/sandbox.rs``; the expected lines are
restated here as literals so a change on either side has to be a deliberate
two-language edit.
"""

from __future__ import annotations

from pathlib import Path

import pytest

import ados.plugins.systemd as systemd_mod
from ados.plugins.manifest import PluginManifest
from ados.plugins.systemd import (
    DEVICE_CAP_RULES,
    render_unit,
    sandbox_directives,
    sandbox_enforced_caps,
)

_INSTALL_DIR = __import__("pathlib").Path("/var/ados/plugins")

_MANIFEST = """
schema_version: 2
id: com.example.sandbox
version: "1.0.0"
name: Sandbox
license: GPL-3.0-or-later
risk: medium
compatibility:
  ados_version: ">=0.1.0,<99.0.0"
agent:
  entrypoint: "plugin:Sandbox"
  isolation: subprocess
  permissions:
    - id: hardware.i2c
    - id: hardware.usb.uvc
    - id: network.outbound
    - id: filesystem.host
"""


def _manifest() -> PluginManifest:
    return PluginManifest.from_yaml_text(_MANIFEST)


def test_no_grant_gives_a_private_dev_and_no_outbound_sockets() -> None:
    lines = sandbox_directives([], True)
    assert "PrivateDevices=yes" in lines
    assert "RestrictAddressFamilies=AF_UNIX" in lines
    assert "IPAddressDeny=any" in lines
    assert not [line for line in lines if line.startswith("DeviceAllow=")]


def test_granting_i2c_does_not_also_open_the_camera() -> None:
    lines = sandbox_directives(["hardware.i2c"], True)
    assert "DevicePolicy=closed" in lines
    assert "DeviceAllow=char-i2c rw" in lines
    # The narrowness is the point: a plugin approved for the I2C bus must not
    # get the camera or the SPI display along with it.
    assert not [line for line in lines if "char-video4linux" in line]
    assert not [line for line in lines if "char-spidev" in line]
    # PrivateDevices would hide the very node the grant unlocked.
    assert "PrivateDevices=yes" not in lines


def test_network_grant_opens_the_inet_families_when_the_loopback_guard_is_active() -> None:
    """Restated as the literals ``ados-plugin-host/src/sandbox.rs`` also pins.

    No systemd address filter: it would also cut ingress to the plugin's own
    listener. The plugin host's nftables guard closes the agent ports instead.
    """
    lines = sandbox_directives(["network.outbound"], True)
    assert "RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK" in lines
    assert not [line for line in lines if line.startswith("IPAddressDeny=")]
    assert not [line for line in lines if line.startswith("IPAddressAllow=")]


def test_network_grant_without_the_loopback_guard_keeps_the_no_grant_socket_policy() -> None:
    lines = sandbox_directives(["network.outbound"], False)
    assert "RestrictAddressFamilies=AF_UNIX" in lines
    assert "IPAddressDeny=any" in lines
    assert lines == sandbox_directives([], False)


def test_the_guard_verdict_is_read_from_the_plugin_host_sidecar(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    sidecar = tmp_path / "plugin-loopback-guard.json"
    monkeypatch.setattr(systemd_mod, "PLUGIN_LOOPBACK_GUARD_JSON", sidecar)
    assert systemd_mod.plugin_loopback_guard_active() is False
    sidecar.write_text("not json")
    assert systemd_mod.plugin_loopback_guard_active() is False
    sidecar.write_text('{"active": false, "reason": "nft missing"}')
    assert systemd_mod.plugin_loopback_guard_active() is False
    sidecar.write_text('{"active": true, "reason": ""}')
    assert systemd_mod.plugin_loopback_guard_active() is True


def test_filesystem_grant_moves_the_data_roots_from_blocked_to_writable() -> None:
    without = sandbox_directives([], True)
    with_grant = sandbox_directives(["filesystem.host"], True)
    rw_without = next(line for line in without if line.startswith("ReadWritePaths="))
    rw_with = next(line for line in with_grant if line.startswith("ReadWritePaths="))
    assert "/mnt" not in rw_without
    assert "/mnt" in rw_with
    inacc_without = next(line for line in without if line.startswith("InaccessiblePaths="))
    inacc_with = next(line for line in with_grant if line.startswith("InaccessiblePaths="))
    assert "-/mnt" in inacc_without
    assert "-/mnt" not in inacc_with
    # The HMAC issuer secret a plugin could mint another plugin's token from is
    # off limits in BOTH postures. `filesystem.host` is broad host access, not
    # access to the host's plugin trust store.
    assert "-/etc/ados/secrets" in inacc_without
    assert "-/etc/ados/secrets" in inacc_with


def test_every_sandbox_capability_changes_the_rendered_unit(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """The load-bearing assertion behind the ``enforced = true`` rows.

    A capability that renders identically granted and ungranted is decorative
    again, with a catalog flag vouching for it. Rendered with the loopback
    guard loaded, the only posture in which ``network.outbound`` is grantable.
    """
    monkeypatch.setattr(systemd_mod, "plugin_loopback_guard_active", lambda: True)
    baseline = render_unit(_manifest(), _INSTALL_DIR, ())
    for cap in sorted(sandbox_enforced_caps()):
        granted = render_unit(_manifest(), _INSTALL_DIR, [cap])
        assert granted != baseline, (
            f"{cap} is declared sandbox-enforced but changes nothing in the unit"
        )


def test_revoking_a_capability_restores_the_denying_unit() -> None:
    """Grant then revoke must return the unit to the refusing form.

    This is what makes a runtime revoke real for a device capability: the
    supervisor re-renders and restarts, and unless the rendered text actually
    reverts, the plugin keeps the device it was told it lost.
    """
    denied = render_unit(_manifest(), _INSTALL_DIR, ())
    granted = render_unit(_manifest(), _INSTALL_DIR, ["hardware.i2c"])
    assert "DeviceAllow=char-i2c rw" in granted
    revoked = render_unit(_manifest(), _INSTALL_DIR, ())
    assert "DeviceAllow=char-i2c rw" not in revoked
    assert revoked == denied


def test_the_same_grant_set_renders_identically() -> None:
    """Order-independent and stable, so an unchanged grant set re-renders to
    the same bytes and the supervisor can skip the plugin restart."""
    a = render_unit(_manifest(), _INSTALL_DIR, ["hardware.i2c", "network.outbound"])
    b = render_unit(_manifest(), _INSTALL_DIR, ["network.outbound", "hardware.i2c"])
    assert a == b


def test_uvc_and_csi_together_emit_one_video4linux_rule() -> None:
    lines = sandbox_directives(["hardware.usb.uvc", "hardware.camera.csi"], True)
    assert lines.count("DeviceAllow=char-video4linux rw") == 1
    assert "DeviceAllow=char-dri rw" in lines


def test_device_rules_match_the_rust_renderer() -> None:
    """Byte parity with ``ados-plugin-host/src/sandbox.rs::DEVICE_CAP_RULES``.

    Restated as a literal rather than imported: either lifecycle path may have
    written a given unit, so a divergence would mean a plugin's sandbox depends
    on which surface the operator installed from — and nothing else in the
    system would notice.
    """
    expected = {
        "hardware.uart": ("char-ttyUSB rw", "char-ttyACM rw", "char-tty rw"),
        "hardware.i2c": ("char-i2c rw",),
        "hardware.spi": ("char-spidev rw",),
        "hardware.gpio": ("char-gpiochip rw",),
        "hardware.usb": ("char-usb_device rw",),
        "hardware.usb.uvc": ("char-video4linux rw",),
        "hardware.camera.csi": ("char-video4linux rw", "char-dri rw"),
    }
    assert dict(DEVICE_CAP_RULES) == expected


def test_the_agent_run_dir_is_hidden_like_the_rust_renderer() -> None:
    """Byte parity with the no-grant filesystem lines ``ados-plugin-host/src/
    sandbox.rs`` pins, and the run dir stays hidden whatever is granted.

    The run dir holds every agent command socket and every plugin's socket
    directory. A plugin that could open one would act with the agent's
    authority, or another plugin's grants, rather than its own. It is hidden
    as a whole so a socket re-created after the plugin started stays hidden.
    """
    expected = [
        "TemporaryFileSystem=/run/ados:ro",
        "BindReadOnlyPaths=-/run/ados/logd.sock",
        "ReadWritePaths=/var/ados/plugin-data /var/log/ados/plugins",
        "ProtectHome=yes",
        "InaccessiblePaths=-/etc/ados/secrets -/etc/ados/plugin-keys "
        "-/srv -/mnt -/media -/boot",
    ]
    lines = sandbox_directives([], True)
    assert lines[-len(expected) :] == expected

    everything = sandbox_directives(sandbox_enforced_caps(), True)
    assert "TemporaryFileSystem=/run/ados:ro" in everything
    for line in everything:
        if line.startswith("ReadWritePaths="):
            assert "/run/ados" not in line
        if line.startswith(("BindReadOnlyPaths=", "BindPaths=")):
            assert line == "BindReadOnlyPaths=-/run/ados/logd.sock"


def test_a_unit_binds_only_its_own_socket_dir() -> None:
    """Each plugin unit binds back exactly one plugin directory: its own."""
    run_dir = systemd_mod.PLUGIN_RUN_DIR
    unit = render_unit(_manifest(), _INSTALL_DIR, ())
    binds = [
        line.removeprefix("BindReadOnlyPaths=")
        for line in unit.splitlines()
        if line.startswith("BindReadOnlyPaths=") and "logd.sock" not in line
    ]
    assert binds == [str(run_dir / "com.example.sandbox")]
    assert (
        f"Environment=ADOS_PLUGIN_SOCKET={run_dir / 'com.example.sandbox' / 'host.sock'}"
        in unit
    )


def test_the_unit_carries_no_start_rate_limit() -> None:
    """A plugin whose host socket is not up yet must keep retrying.

    ``StartLimitBurst=5`` in a 60 s window meant five quick failures left the
    unit in a failed state an operator had to clear by hand — a terminal state
    for a condition that resolves itself within seconds.
    """
    unit = render_unit(_manifest(), _INSTALL_DIR, ())
    assert "StartLimitIntervalSec=0" in unit
    assert "StartLimitBurst" not in unit
