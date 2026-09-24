"""batman-adv local wireless mesh lifecycle for relay/receiver roles.

Brings up a second wireless interface in 802.11s (preferred) or IBSS
(fallback) mode, binds it to `bat0`, and drives batman-adv gateway
mode based on role + cloud_uplink config, then holds the mesh up until
stopped. Neighbor, gateway and partition state is polled and published by
the native groundlink mesh loop (``/run/ados/mesh-state.json`` and the
mesh-event journal), not here.

Systemd unit is `ados-batman.service`, gated on the mesh role sentinel
`/etc/ados/mesh/role`. On direct-mode nodes the unit stays inactive.

Non-goals for this module:

- Pairing. That lives in `pairing_manager`; we only consume the mesh_id
  and shared key it writes.
- WFB fragment forwarding. That is `wfb_relay` / `wfb_receiver`.
- Cloud uplink bringup. `uplink_router` owns the decision; we read the
  result and advertise it as a batman gateway when local.
- Publishing `/run/ados/mesh-state.json`. The native groundlink mesh poll
  loop (`ados-groundlink`, started by the same relay/receiver role) is the
  sidecar's single writer and stamps the schema `version` its readers check.
  This module wrote the same path every 2 s from a second process, so the
  file's content came down to which write landed last and a version-less
  body tripped every reader's drift warning.

This service shells out to userland tools (`batctl`, `iw`, `ip`,
`modprobe`, `wpa_supplicant`). pyroute2 was considered but the existing
agent stack uses subprocess everywhere; staying with that avoids a new
kernel netlink dependency on the installer.
"""

from __future__ import annotations

import asyncio
import hashlib
import json
import os
import secrets
import signal
import subprocess
import sys
from pathlib import Path

import structlog

from ados.core.config import ADOSConfig, load_config
from ados.core.logging import configure_logging, get_logger
from ados.core.paths import (
    MESH_GATEWAY_JSON,
    MESH_ROLE_PATH,
    UPLINK_ACTIVE_FLAG,
)
from ados.core.paths import (
    MESH_ID_PATH as _MESH_ID_PATH,
)
from ados.core.paths import (
    MESH_PSK_PATH as _MESH_PSK_PATH,
)

from .role_manager import get_current_role

log = get_logger("ground_station.mesh_manager")

MESH_ID_PATH = _MESH_ID_PATH
MESH_PSK_PATH = _MESH_PSK_PATH

_GATEWAY_BANDWIDTH_DEFAULT = "10000/2000"  # 10 Mbps down, 2 Mbps up hint


def _run(cmd: list[str], timeout: float = 10.0) -> tuple[int, str, str]:
    """Run a command with a hard timeout. Escalates TERM -> KILL.

    `subprocess.run(timeout=...)` calls `proc.kill()` internally on
    timeout but then waits for `communicate()` to drain stdout/stderr.
    If the process is deadlocked in the kernel (wedged WiFi driver,
    stuck `batctl`), that drain can itself hang. We use Popen directly
    so a final forced wait + resource release is bounded.
    """
    try:
        proc = subprocess.Popen(
            cmd,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
    except FileNotFoundError:
        return 127, "", "not found"

    try:
        stdout, stderr = proc.communicate(timeout=timeout)
        return proc.returncode, stdout, stderr
    except subprocess.TimeoutExpired:
        proc.terminate()
        try:
            stdout, stderr = proc.communicate(timeout=1.0)
        except subprocess.TimeoutExpired:
            proc.kill()
            try:
                stdout, stderr = proc.communicate(timeout=1.0)
            except subprocess.TimeoutExpired:
                # Kernel still holds the process. Give up and let the
                # zombie be reaped when the parent exits. We have
                # exhausted the recovery options without blocking the
                # caller further.
                return 124, "", "timeout (kill did not release)"
        return 124, stdout or "", stderr or "timeout"


class MeshIdentityMissing(RuntimeError):
    """A relay role was requested but the deployment mesh identity
    (mesh_id + PSK) has not been delivered via a pairing invite yet.
    The caller should fall back to `direct` rather than crash-loop."""


def _ensure_mesh_identity(role: str, config: ADOSConfig) -> tuple[str, bytes]:
    """Load or create the deployment mesh_id + shared PSK.

    On a receiver node the first boot generates both and writes them to
    `/etc/ados/mesh/`. Relays pick these values up from the pairing
    invite bundle written by `pairing_manager`. If the files are missing
    on a relay we raise `MeshIdentityMissing` so the caller can surface
    an OLED error state and downgrade role instead of crash-looping.
    """
    MESH_ID_PATH.parent.mkdir(parents=True, exist_ok=True)

    configured_id = config.ground_station.mesh.mesh_id
    if configured_id:
        mesh_id = configured_id
    elif MESH_ID_PATH.is_file():
        mesh_id = MESH_ID_PATH.read_text(encoding="utf-8").strip()
    elif role == "receiver":
        # Derive a stable short id from the device_id. HKDF would be
        # overkill for a 16-char mesh SSID; a SHA-256 truncation keeps
        # it deterministic per device.
        seed = config.agent.device_id or secrets.token_hex(8)
        mesh_id = "ados-" + hashlib.sha256(seed.encode()).hexdigest()[:10]
        MESH_ID_PATH.write_text(mesh_id + "\n", encoding="utf-8")
        os.chmod(MESH_ID_PATH, 0o644)
    else:
        raise MeshIdentityMissing(
            "mesh_id missing. A relay must be paired with a receiver before "
            "mesh_manager can start."
        )

    psk_path = Path(config.ground_station.mesh.shared_key_path)
    if psk_path.is_file():
        psk = psk_path.read_bytes().strip()
        if len(psk) < 16:
            raise RuntimeError(
                f"mesh PSK at {psk_path} is shorter than 16 bytes"
            )
    elif role == "receiver":
        psk = secrets.token_bytes(32)
        psk_path.parent.mkdir(parents=True, exist_ok=True)
        psk_path.write_bytes(psk)
        os.chmod(psk_path, 0o600)
    else:
        raise MeshIdentityMissing(
            f"mesh PSK missing at {psk_path}. A relay must be paired before "
            "mesh_manager can start."
        )

    return mesh_id, psk


def _pick_mesh_iface(configured: str | None) -> str | None:
    """Return the wireless interface batman-adv should bind to.

    Priority: explicit config > the mesh role from the single driver-keyed
    role resolver. The resolver never assigns the WFB flight radio to the
    mesh (it is by construction a non-WFB radio), and decides by the bound
    kernel driver, not by the racing interface name.
    """
    if configured:
        return configured

    from ados.services.network.interface_roles import assign_roles

    roles = assign_roles()
    # mesh is the second non-WFB radio; fall back to the single control radio
    # (mgmt_wifi) when the box has exactly one non-WFB adapter.
    return roles.mesh or roles.mgmt_wifi


def _modprobe_batman() -> bool:
    rc, _out, err = _run(["modprobe", "batman-adv"], timeout=10.0)
    if rc != 0:
        log.error("modprobe_batman_failed", err=err.strip())
        return False
    return True


def _bring_up_mesh_iface(
    iface: str,
    carrier: str,
    mesh_id: str,
    psk: bytes,
    channel: int,
) -> bool:
    """Configure the mesh-side wireless interface in 802.11s or IBSS mode."""
    # Flush the interface first to a clean baseline.
    _run(["ip", "link", "set", iface, "down"], timeout=5.0)
    _run(["iw", "dev", iface, "disconnect"], timeout=5.0)

    if carrier == "802.11s":
        # Set mesh type. Some drivers require `mesh` explicitly.
        rc, _o, e = _run(["iw", "dev", iface, "set", "type", "mp"], timeout=5.0)
        if rc != 0:
            log.warning("iw_set_type_mp_failed", iface=iface, err=e.strip())
        _run(["ip", "link", "set", iface, "up"], timeout=5.0)
        # 2.4 GHz channel to frequency. Channel 1 is 2412 MHz.
        freq_mhz = 2407 + channel * 5 if 1 <= channel <= 13 else 2412
        rc, _o, e = _run(
            ["iw", "dev", iface, "mesh", "join", mesh_id, "freq", str(freq_mhz), "HT20"],
            timeout=10.0,
        )
        if rc != 0:
            log.error("iw_mesh_join_failed", iface=iface, err=e.strip())
            return False
    elif carrier == "ibss":
        rc, _o, e = _run(["iw", "dev", iface, "set", "type", "ibss"], timeout=5.0)
        if rc != 0:
            log.warning("iw_set_type_ibss_failed", iface=iface, err=e.strip())
        _run(["ip", "link", "set", iface, "up"], timeout=5.0)
        freq_mhz = 2407 + channel * 5 if 1 <= channel <= 13 else 2412
        rc, _o, e = _run(
            ["iw", "dev", iface, "ibss", "join", mesh_id, str(freq_mhz), "HT20"],
            timeout=10.0,
        )
        if rc != 0:
            log.error("iw_ibss_join_failed", iface=iface, err=e.strip())
            return False
    else:
        log.error("unknown_carrier", carrier=carrier)
        return False

    return True


def _bind_iface_to_bat(iface: str, bat_iface: str) -> bool:
    rc, _o, e = _run(["batctl", "if", "add", iface], timeout=5.0)
    if rc != 0 and "already" not in e.lower():
        log.error("batctl_if_add_failed", iface=iface, err=e.strip())
        return False
    rc, _o, e = _run(["ip", "link", "set", bat_iface, "up"], timeout=5.0)
    if rc != 0:
        log.error("bat_iface_up_failed", iface=bat_iface, err=e.strip())
        return False
    return True


_GATEWAY_PREFERENCE_PATH = MESH_GATEWAY_JSON


def _apply_persisted_gateway_preference() -> str | None:
    """Re-apply the operator's last gateway pin on mesh setup.

    Reads `/etc/ados/mesh/gateway.json` which is written by the REST
    endpoint when the operator clicks a pin button in the GCS.
    Returns the pinned MAC on success, or None if no preference is
    persisted or the file is unreadable.

    The file schema is `{"mode": "auto"|"pinned"|"off", "pinned_mac": str|null}`.
    Only the "pinned" mode with a non-null MAC triggers `batctl gw_sel`.
    Auto and off modes are already the default batman-adv behavior on
    fresh bringup; no explicit action needed here.
    """
    if not _GATEWAY_PREFERENCE_PATH.is_file():
        return None
    try:
        data = json.loads(_GATEWAY_PREFERENCE_PATH.read_text(encoding="utf-8"))
    except (OSError, ValueError) as exc:
        log.warning("gateway_preference_read_failed", error=str(exc))
        return None
    mode = data.get("mode")
    pinned_mac = data.get("pinned_mac")
    if mode != "pinned" or not pinned_mac:
        return None
    rc, _o, err = _run(["batctl", "gw_sel", str(pinned_mac)], timeout=5.0)
    if rc != 0:
        log.warning(
            "gateway_pin_apply_failed",
            mac=pinned_mac,
            err=err.strip(),
        )
        return None
    log.info("gateway_pin_applied", mac=pinned_mac)
    return str(pinned_mac)


def _configure_gateway_mode(role: str, cloud_uplink: str, has_uplink: bool) -> str:
    """Pick batman gateway mode and apply it. Returns the resulting mode."""
    advertise = False
    if cloud_uplink == "force_on":
        advertise = True
    elif cloud_uplink == "force_off":
        advertise = False
    else:  # auto
        advertise = has_uplink

    if advertise:
        mode = "server"
        _run(
            ["batctl", "gw_mode", "server", _GATEWAY_BANDWIDTH_DEFAULT],
            timeout=5.0,
        )
    elif role == "receiver":
        mode = "client"
        _run(["batctl", "gw_mode", "client"], timeout=5.0)
    else:
        mode = "off"
        _run(["batctl", "gw_mode", "off"], timeout=5.0)
    return mode


class MeshManager:
    """Main service class. One instance per process."""

    def __init__(self, config: ADOSConfig) -> None:
        self._config = config
        self._role = get_current_role()
        self._bat_iface = config.ground_station.mesh.bat_iface
        self._mesh_iface = ""
        self._carrier = config.ground_station.mesh.carrier
        self._channel = config.ground_station.mesh.channel
        self._mesh_id = ""

    async def setup(self) -> bool:
        """One-shot bringup. Returns True on success."""
        if self._role not in ("relay", "receiver"):
            log.warning("mesh_skip_direct_role")
            return False

        try:
            mesh_id, _psk = _ensure_mesh_identity(self._role, self._config)
        except MeshIdentityMissing as exc:
            log.error("mesh_identity_missing", error=str(exc))
            # Signal a distinct "graceful downgrade" path to main() by
            # re-raising. A plain setup-failure would have returned False
            # and triggered a systemd restart loop.
            raise
        except RuntimeError as exc:
            log.error("mesh_identity_error", error=str(exc))
            return False
        self._mesh_id = mesh_id

        iface = _pick_mesh_iface(self._config.ground_station.mesh.interface_override)
        if not iface:
            log.error("mesh_iface_not_found")
            return False
        self._mesh_iface = iface

        if not _modprobe_batman():
            return False

        ok = _bring_up_mesh_iface(
            iface, self._carrier, mesh_id, _psk, self._channel,
        )
        if not ok:
            return False

        if not _bind_iface_to_bat(iface, self._bat_iface):
            return False

        # Gateway mode decision. "has_uplink" is best-effort here;
        # uplink_router owns the real decision. Operator preference from
        # the GCS lands in /etc/ados/mesh/gateway.json and is re-applied
        # here at setup so pins survive agent + mesh restarts.
        has_uplink = UPLINK_ACTIVE_FLAG.is_file()
        mode = _configure_gateway_mode(
            self._role,
            self._config.ground_station.cloud_uplink,
            has_uplink,
        )
        pinned_mac = _apply_persisted_gateway_preference()
        log.info(
            "mesh_up",
            role=self._role,
            mesh_iface=iface,
            carrier=self._carrier,
            mesh_id=mesh_id,
            gw_mode=mode,
            pinned_mac=pinned_mac,
        )
        return True

    async def teardown(self) -> None:
        if self._mesh_iface:
            _run(["batctl", "if", "del", self._mesh_iface], timeout=5.0)
            _run(["iw", "dev", self._mesh_iface, "disconnect"], timeout=5.0)
            _run(["ip", "link", "set", self._mesh_iface, "down"], timeout=5.0)
        _run(["ip", "link", "set", self._bat_iface, "down"], timeout=5.0)


async def main() -> None:
    config = load_config()
    configure_logging(config.logging.level)
    slog = structlog.get_logger()
    slog.info("mesh_manager_starting")

    manager = MeshManager(config)
    # Direct-role nodes have no mesh to bring up. The systemd unit's
    # AssertPathExists gate only checks that the role sentinel file
    # exists, not that it reads `relay`/`receiver`, so this process can
    # still be launched on a direct node. A no-op is success, not a
    # crash: exit 0, the one status the unit's RestartPreventExitStatus=0
    # exempts from Restart=always. Genuine setup failures
    # on relay/receiver still fall through to the exit-2 path below.
    if manager._role not in ("relay", "receiver"):
        slog.info("mesh_not_applicable_for_role", role=manager._role)
        sys.exit(0)
    try:
        ok = await manager.setup()
    except MeshIdentityMissing:
        # Relay role was active but no pairing invite bundle has been
        # delivered yet. Crash-looping helps nobody, so we downgrade the
        # role sentinel to `direct`, let systemd's ConditionPathExists
        # keep us inactive until the role sentinel is re-armed (via
        # pairing or explicit role changes), and exit cleanly.
        try:
            role_path = MESH_ROLE_PATH
            role_path.parent.mkdir(parents=True, exist_ok=True)
            role_path.write_text("direct\n", encoding="utf-8")
        except OSError as exc:
            slog.error("mesh_role_sentinel_write_failed", error=str(exc))
        slog.warning("mesh_identity_missing_downgraded_to_direct")
        sys.exit(0)
    if not ok:
        slog.error("mesh_setup_failed")
        sys.exit(2)

    shutdown = asyncio.Event()
    loop = asyncio.get_event_loop()
    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, shutdown.set)

    await shutdown.wait()

    slog.info("mesh_manager_stopping")
    await manager.teardown()
    slog.info("mesh_manager_stopped")


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
    sys.exit(0)
