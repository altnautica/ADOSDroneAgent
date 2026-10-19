"""batman-adv local wireless mesh lifecycle for relay/receiver roles.

Brings up a second wireless interface in 802.11s (preferred) or IBSS
(fallback) mode with authenticated keying (SAE for 802.11s, IBSS-RSN for
IBSS, both through wpa_supplicant keyed from the deployment PSK), binds it to
`bat0`, and drives batman-adv gateway mode based on role + cloud_uplink
config. A fixed-cadence supervision loop then re-joins and re-binds the mesh
when the interface, its wpa_supplicant, the join or the batman membership is
lost, and re-evaluates the gateway mode as the uplink comes and goes. There is
no open-join path: if the secure join cannot be brought up the node stays off
the mesh. Neighbor, gateway and partition state is polled and published by
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
import json
import signal
import subprocess
import sys
import time
from pathlib import Path

import structlog

from ados.core.config import ADOSConfig, load_config
from ados.core.logging import configure_logging, get_logger
from ados.core.paths import (
    ADOS_RUN_DIR,
    SUPERVISOR_SOCK,
    UPLINK_ACTIVE_FLAG,
)
from ados.core.paths import (
    MESH_ID_PATH as _MESH_ID_PATH,
)
from ados.core.paths import (
    MESH_PSK_PATH as _MESH_PSK_PATH,
)

from ._mesh_cmd import (
    _GATEWAY_PREFERENCE_PATH as _GATEWAY_PREFERENCE_PATH,
)
from ._mesh_cmd import (
    _apply_persisted_gateway_preference,
    _modprobe_batman,
    _run,
)
from ._mesh_conf import (
    _WPA_STOP_GRACE_S as _WPA_STOP_GRACE_S,
)
from ._mesh_conf import (
    _channel_freq_mhz,
    _gateway_mode_for,
    _mesh_passphrase,
    _stop_wpa_supplicant,
    _wpa_supplicant_conf,
    _write_private,
)
from ._mesh_conf import (
    _check_conf_value as _check_conf_value,
)
from ._mesh_identity import (
    MeshIdentityMissing,
    _ensure_mesh_identity,
    _pick_mesh_iface,
    _published_mesh_adapter,
)
from .role_manager import get_current_role

log = get_logger("ground_station.mesh_manager")

MESH_ID_PATH = _MESH_ID_PATH
MESH_PSK_PATH = _MESH_PSK_PATH

_GATEWAY_BANDWIDTH_DEFAULT = "10000/2000"  # 10 Mbps down, 2 Mbps up hint

# Where the per-interface wpa_supplicant config (it carries the mesh passphrase)
# is written, 0700 directory / 0600 file.
_WPA_RUN_DIR = ADOS_RUN_DIR / "mesh"
_SYS_CLASS_NET = Path("/sys/class/net")

# How long a fresh wpa_supplicant gets to show the interface joined.
_JOIN_VERIFY_TIMEOUT_S = 15.0
_JOIN_VERIFY_POLL_S = 0.5

# Supervision cadence. Fixed on purpose: the loop never backs off and never
# gives up, so a replugged dongle or a returning uplink is picked up within one
# interval however long the outage lasted.
_SUPERVISE_INTERVAL_S = 3.0

# Per carrier: the `iw ... set type` argument and the `iw dev <if> info` type.
_IW_SET_TYPE = {"802.11s": "mp", "ibss": "ibss"}
_IW_INFO_TYPE = {"802.11s": "mesh point", "ibss": "IBSS"}


# Bound on the supervisor role request. The supervisor may stop this unit as
# part of the transition, so the reply is only waited on briefly.
_ROLE_REQUEST_TIMEOUT_S = 5.0


async def _request_direct_role(sock_path: str | None = None) -> None:
    """Ask the supervisor to move this node to ``direct``.

    Role transitions run in the supervisor, never in a role unit: it stops and
    starts the role's units, flips the role sentinel and persists the role, so
    the sentinel and ``ground_station.role`` stay in agreement. Best-effort: an
    unreachable supervisor is logged and the caller still exits cleanly.
    """
    path = sock_path or SUPERVISOR_SOCK
    request = {"op": "set_role", "role": "direct", "reason": "mesh_identity_missing"}
    try:
        reader, writer = await asyncio.wait_for(
            asyncio.open_unix_connection(path), timeout=_ROLE_REQUEST_TIMEOUT_S
        )
    except (TimeoutError, OSError) as exc:
        log.error("mesh_role_downgrade_unreachable", error=str(exc))
        return
    try:
        writer.write((json.dumps(request) + "\n").encode("utf-8"))
        await writer.drain()
        try:
            line = await asyncio.wait_for(
                reader.readline(), timeout=_ROLE_REQUEST_TIMEOUT_S
            )
        except (TimeoutError, OSError):
            line = b""
    finally:
        writer.close()
        try:
            await writer.wait_closed()
        except (OSError, ConnectionError):
            pass
    log.info("mesh_role_downgrade_requested", reply=line.decode("utf-8", "replace").strip())


def _wpa_conf_path(iface: str) -> Path:
    return _WPA_RUN_DIR / f"wpa_supplicant-{iface}.conf"


def _iface_joined(iface: str, carrier: str, mesh_id: str) -> bool:
    """True when ``iw dev <iface> info`` shows the carrier's interface type
    joined to ``mesh_id`` (iw reports the mesh ID / IBSS SSID as ``ssid``)."""
    rc, out, _e = _run(["iw", "dev", iface, "info"], timeout=5.0)
    if rc != 0:
        return False
    iftype = ssid = None
    for line in out.splitlines():
        field = line.strip()
        if field.startswith("type "):
            iftype = field[len("type "):].strip()
        elif field.startswith("ssid "):
            ssid = field[len("ssid "):].strip()
    return iftype == _IW_INFO_TYPE.get(carrier) and ssid == mesh_id


def _bat_member(iface: str, bat_iface: str) -> bool:
    """True when ``iface`` is a batman-adv hard interface of ``bat_iface``."""
    try:
        member_of = (_SYS_CLASS_NET / iface / "batman_adv" / "mesh_iface").read_text()
    except OSError:
        return False
    return member_of.strip() == bat_iface


def _bring_up_mesh_iface(
    iface: str,
    carrier: str,
    mesh_id: str,
    psk: bytes,
    channel: int,
) -> subprocess.Popen[bytes] | None:
    """Join the mesh with authenticated keying, or refuse to join at all.

    Writes the wpa_supplicant config (0600, under the run dir), starts
    wpa_supplicant on the interface and waits for ``iw`` to show it joined to
    ``mesh_id``. Returns the running wpa_supplicant for the caller to track, or
    None when the secure join could not be brought up. There is deliberately
    no fallback to an open ``iw ... join``: a mesh without keying would let any
    radio that knows the mesh_id onto the batman-adv fabric.
    """
    set_type = _IW_SET_TYPE.get(carrier)
    if set_type is None:
        log.error("unknown_carrier", carrier=carrier)
        return None
    try:
        conf = _wpa_supplicant_conf(
            carrier, mesh_id, _mesh_passphrase(psk), _channel_freq_mhz(channel)
        )
    except ValueError as exc:
        log.error("mesh_identity_invalid", error=str(exc))
        return None
    conf_path = _wpa_conf_path(iface)
    try:
        _write_private(conf_path, conf)
    except OSError as exc:
        log.error("mesh_wpa_conf_write_failed", path=str(conf_path), error=str(exc))
        return None

    _run(["ip", "link", "set", iface, "down"], timeout=5.0)
    rc, _o, e = _run(["iw", "dev", iface, "set", "type", set_type], timeout=5.0)
    if rc != 0:
        log.warning("iw_set_type_failed", iface=iface, type=set_type, err=e.strip())
    _run(["ip", "link", "set", iface, "up"], timeout=5.0)

    try:
        proc = subprocess.Popen(
            ["wpa_supplicant", "-i", iface, "-D", "nl80211", "-c", str(conf_path)],
            stdin=subprocess.DEVNULL,
        )
    except OSError as exc:
        log.error("wpa_supplicant_spawn_failed", iface=iface, error=str(exc))
        return None

    deadline = time.monotonic() + _JOIN_VERIFY_TIMEOUT_S
    while True:
        if proc.poll() is not None:
            log.error("wpa_supplicant_exited", iface=iface, rc=proc.returncode)
            return None
        if _iface_joined(iface, carrier, mesh_id):
            return proc
        if time.monotonic() >= deadline:
            break
        time.sleep(_JOIN_VERIFY_POLL_S)
    log.error("mesh_secure_join_unverified", iface=iface, carrier=carrier)
    _stop_wpa_supplicant(proc)
    return None


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


def _apply_gateway_mode(mode: str) -> bool:
    cmd = ["batctl", "gw_mode", mode]
    if mode == "server":
        cmd.append(_GATEWAY_BANDWIDTH_DEFAULT)
    rc, _o, err = _run(cmd, timeout=5.0)
    if rc != 0:
        log.warning("gw_mode_apply_failed", mode=mode, err=err.strip())
        return False
    return True


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
        self._psk = b""
        # The wpa_supplicant holding the secure join, tracked so supervision
        # notices it dying.
        self._wpa: subprocess.Popen[bytes] | None = None
        # The gateway mode last applied; None until one applied successfully.
        self._gw_mode: str | None = None
        # The last fault supervision saw, so a lasting outage logs once.
        self._fault: str | None = None

    async def setup(self) -> bool:
        """Initial bringup. Returns True on success."""
        if self._role not in ("relay", "receiver"):
            log.warning("mesh_skip_direct_role")
            return False

        try:
            mesh_id, psk = _ensure_mesh_identity(self._role, self._config)
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
        self._psk = psk

        iface = _pick_mesh_iface(
            self._config.ground_station.mesh.interface_override, self._role
        )
        if not iface:
            log.error("mesh_iface_not_found")
            return False
        self._mesh_iface = iface

        if not _modprobe_batman():
            return False

        if not self._join_and_bind():
            return False

        # Operator preference from the GCS lands in
        # /etc/ados/mesh/gateway.json and is re-applied here at setup so pins
        # survive agent + mesh restarts.
        mode = self._update_gateway_mode()
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

    def _stop_wpa(self) -> None:
        if self._wpa is not None:
            _stop_wpa_supplicant(self._wpa)
            self._wpa = None

    def _join_and_bind(self) -> bool:
        """(Re)run the secure join and the batman bind on the mesh iface."""
        self._stop_wpa()
        proc = _bring_up_mesh_iface(
            self._mesh_iface, self._carrier, self._mesh_id, self._psk, self._channel,
        )
        if proc is None:
            return False
        self._wpa = proc
        return _bind_iface_to_bat(self._mesh_iface, self._bat_iface)

    def _mesh_fault(self) -> str | None:
        """Why the mesh is not up as configured, or None when it is."""
        iface = self._mesh_iface
        if not iface or not (_SYS_CLASS_NET / iface).exists():
            return "iface_missing"
        if self._reassigned_iface() is not None:
            return "iface_reassigned"
        if self._wpa is None or self._wpa.poll() is not None:
            return "wpa_supplicant_dead"
        if not _iface_joined(iface, self._carrier, self._mesh_id):
            return "not_joined"
        if not _bat_member(iface, self._bat_iface):
            return "not_in_bat"
        return None

    def _reassigned_iface(self) -> str | None:
        """The adapter the native resolver now assigns to the mesh, when it is
        present and differs from the one in use. The native relay / receiver
        loop publishes it once it has resolved the WFB adapters, which can be
        after this service picked its fallback; an operator interface override
        always wins and is never reassigned."""
        if self._config.ground_station.mesh.interface_override:
            return None
        published = _published_mesh_adapter(self._role)
        if (
            published
            and published != self._mesh_iface
            and (_SYS_CLASS_NET / published).exists()
        ):
            return published
        return None

    def _update_gateway_mode(self) -> str | None:
        """Re-decide the gateway mode from the uplink flag; run `batctl` only
        when the decision differs from the mode last applied."""
        mode = _gateway_mode_for(
            self._role,
            self._config.ground_station.cloud_uplink,
            UPLINK_ACTIVE_FLAG.is_file(),
        )
        if mode != self._gw_mode and _apply_gateway_mode(mode):
            log.info("mesh_gw_mode_applied", previous=self._gw_mode, mode=mode)
            self._gw_mode = mode
        return self._gw_mode

    def tick(self) -> None:
        """One supervision pass: heal the mesh if it was lost, then re-evaluate
        the gateway mode."""
        fault = self._mesh_fault()
        if fault is not None:
            if fault != self._fault:
                log.warning("mesh_link_lost", iface=self._mesh_iface, reason=fault)
            if fault in ("iface_missing", "iface_reassigned"):
                # A replugged dongle can come back under another name, and the
                # native resolver can hand the mesh a different adapter.
                self._stop_wpa()
                if fault == "iface_reassigned" and self._mesh_iface:
                    _run(["batctl", "if", "del", self._mesh_iface], timeout=5.0)
                iface = _pick_mesh_iface(
                    self._config.ground_station.mesh.interface_override, self._role
                )
                if iface and (_SYS_CLASS_NET / iface).exists():
                    self._mesh_iface = iface
                    fault = None if self._join_and_bind() else fault
            else:
                fault = None if self._join_and_bind() else fault
            if fault is None:
                log.info("mesh_rejoined", iface=self._mesh_iface)
        self._fault = fault
        self._update_gateway_mode()

    async def supervise(self, shutdown: asyncio.Event) -> None:
        """Run :meth:`tick` every ``_SUPERVISE_INTERVAL_S`` until ``shutdown``."""
        while not shutdown.is_set():
            try:
                await asyncio.wait_for(shutdown.wait(), timeout=_SUPERVISE_INTERVAL_S)
            except TimeoutError:
                await asyncio.to_thread(self.tick)

    async def teardown(self) -> None:
        self._stop_wpa()
        if self._mesh_iface:
            _run(["batctl", "if", "del", self._mesh_iface], timeout=5.0)
            _run(["ip", "link", "set", self._mesh_iface, "down"], timeout=5.0)
            _wpa_conf_path(self._mesh_iface).unlink(missing_ok=True)
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
        # delivered yet. Crash-looping helps nobody, so ask the supervisor to
        # move the node to `direct` (it owns the transition, the sentinel and
        # the persisted role) and exit cleanly.
        slog.warning("mesh_identity_missing_requesting_direct")
        await _request_direct_role()
        sys.exit(0)
    if not ok:
        slog.error("mesh_setup_failed")
        sys.exit(2)

    shutdown = asyncio.Event()
    loop = asyncio.get_event_loop()
    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, shutdown.set)

    await manager.supervise(shutdown)

    slog.info("mesh_manager_stopping")
    await manager.teardown()
    slog.info("mesh_manager_stopped")


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
    sys.exit(0)
