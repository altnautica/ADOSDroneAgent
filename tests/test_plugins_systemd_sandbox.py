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
    lines = sandbox_directives([])
    assert "PrivateDevices=yes" in lines
    assert "RestrictAddressFamilies=AF_UNIX" in lines
    assert "IPAddressDeny=any" in lines
    assert not [line for line in lines if line.startswith("DeviceAllow=")]


def test_granting_i2c_does_not_also_open_the_camera() -> None:
    lines = sandbox_directives(["hardware.i2c"])
    assert "DevicePolicy=closed" in lines
    assert "DeviceAllow=char-i2c rw" in lines
    # The narrowness is the point: a plugin approved for the I2C bus must not
    # get the camera or the SPI display along with it.
    assert not [line for line in lines if "char-video4linux" in line]
    assert not [line for line in lines if "char-spidev" in line]
    # PrivateDevices would hide the very node the grant unlocked.
    assert "PrivateDevices=yes" not in lines


def test_network_grant_flips_the_address_family_filter_but_keeps_loopback_closed() -> None:
    """Restated as the literal ``ados-plugin-host/src/sandbox.rs`` also pins.

    A granted plugin reaches the network but not the agent's loopback
    listeners, which trust a loopback peer as on-box.
    """
    lines = sandbox_directives(["network.outbound"])
    assert "RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK" in lines
    assert "IPAddressDeny=any" not in lines
    assert "IPAddressDeny=localhost" in lines
    assert not [line for line in lines if line.startswith("IPAddressAllow=")]


def test_filesystem_grant_moves_the_data_roots_from_blocked_to_writable() -> None:
    without = sandbox_directives([])
    with_grant = sandbox_directives(["filesystem.host"])
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


def test_every_sandbox_capability_changes_the_rendered_unit() -> None:
    """The load-bearing assertion behind the ``enforced = true`` rows.

    A capability that renders identically granted and ungranted is decorative
    again, with a catalog flag vouching for it.
    """
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
    lines = sandbox_directives(["hardware.usb.uvc", "hardware.camera.csi"])
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


def test_agent_command_sockets_are_hidden_like_the_rust_renderer() -> None:
    """Byte parity with the no-grant line ``ados-plugin-host/src/sandbox.rs``
    pins, and the sockets stay hidden whatever the plugin is granted.

    A plugin that could open one of these would act with the agent's
    authority rather than its own grants.
    """
    expected = (
        "InaccessiblePaths=-/etc/ados/secrets -/etc/ados/plugin-keys "
        "-/run/ados/plugin-host -/run/ados/control.sock -/run/ados/api-internal.sock "
        "-/run/ados/mavlink.sock -/run/ados/msp.sock -/run/ados/supervisor.sock "
        "-/run/ados/radio-cmd.sock -/run/ados/radio-aux.sock -/run/ados/wfb-cmd.sock "
        "-/run/ados/video-cmd.sock -/run/ados/gpio-cmd.sock -/run/ados/hid-cmd.sock "
        "-/run/ados/pic.sock -/run/ados/crsf-cmd.sock -/run/ados/wifi-cmd.sock "
        "-/run/ados/groundlink-cmd.sock -/run/ados/tunnel-config-cmd.sock "
        "-/run/ados/atlas-control.sock -/run/ados/pairing.sock "
        "-/run/ados/logd-query.sock -/srv -/mnt -/media -/boot"
    )
    lines = sandbox_directives([])
    assert next(line for line in lines if line.startswith("InaccessiblePaths=")) == expected

    everything = sandbox_directives(sandbox_enforced_caps())
    hidden = next(line for line in everything if line.startswith("InaccessiblePaths="))
    for path in ("/run/ados/plugin-host", "/run/ados/control.sock", "/run/ados/gpio-cmd.sock"):
        assert f" -{path}" in hidden
    assert "/run/ados/plugins" not in hidden


def test_the_unit_carries_no_start_rate_limit() -> None:
    """A plugin whose host socket is not up yet must keep retrying.

    ``StartLimitBurst=5`` in a 60 s window meant five quick failures left the
    unit in a failed state an operator had to clear by hand — a terminal state
    for a condition that resolves itself within seconds.
    """
    unit = render_unit(_manifest(), _INSTALL_DIR, ())
    assert "StartLimitIntervalSec=0" in unit
    assert "StartLimitBurst" not in unit
