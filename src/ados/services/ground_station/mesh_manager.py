"""batman-adv local wireless mesh lifecycle for relay/receiver roles.

Brings up a second wireless interface in 802.11s (preferred) or IBSS
(fallback) mode, binds it to `bat0`, and drives batman-adv gateway
mode based on role + cloud_uplink config. Polls neighbors, routes, and
gateways; publishes changes on the shared `MeshEventBus` so the GCS
Hardware tab, OLED status screens, and REST clients stay in sync.

Systemd unit is `ados-batman.service`, gated on the mesh role sentinel
`/etc/ados/mesh/role`. On direct-mode nodes the unit stays inactive.

Non-goals for this module:

- Pairing. That lives in `pairing_manager`; we only consume the mesh_id
  and shared key it writes.
- WFB fragment forwarding. That is `wfb_relay` / `wfb_receiver`.
- Cloud uplink bringup. `uplink_router` owns the decision; we read the
  result and advertise it as a batman gateway when local.

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
import time
from dataclasses import dataclass, field
from pathlib import Path

import structlog

from ados.core.config import ADOSConfig, load_config
from ados.core.logging import configure_logging, get_logger

from .events import MeshEvent, get_mesh_event_bus
from .role_manager import get_current_role

log = get_logger("ground_station.mesh_manager")

MESH_STATE_PATH = Path("/run/ados/mesh.sock")
MESH_STATE_JSON = Path("/run/ados/mesh-state.json")
MESH_ID_PATH = Path("/etc/ados/mesh/id")
MESH_PSK_PATH = Path("/etc/ados/mesh/psk.key")

_POLL_INTERVAL_S = 2.0
_NEIGHBOR_CHURN_DEAD_MS = 5000
_GATEWAY_BANDWIDTH_DEFAULT = "10000/2000"  # 10 Mbps down, 2 Mbps up hint


@dataclass
class MeshNeighbor:
    mac: str
    iface: str
    tq: int
    last_seen_ms: int


@dataclass
class MeshGateway:
    mac: str
    class_up_kbps: int
    class_down_kbps: int
    tq: int
    selected: bool


@dataclass
class MeshSnapshot:
    role: str
    bat_iface: str
    mesh_iface: str
    carrier: str
    mesh_id: str
    up: bool
    neighbors: list[MeshNeighbor] = field(default_factory=list)
    gateways: list[MeshGateway] = field(default_factory=list)
    selected_gateway: str | None = None
    partition: bool = False
    started_at_ms: int = 0
    last_poll_ms: int = 0


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


def _ensure_mesh_identity(role: str, config: ADOSConfig) -> tuple[str, bytes]:
    """Load or create the deployment mesh_id + shared PSK.

    On a receiver node the first boot generates both and writes them to
    `/etc/ados/mesh/`. Relays pick these values up from the pairing
    invite bundle written by `pairing_manager`. If the files are missing
    on a relay we raise so the caller can surface an OLED error state.
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
        raise RuntimeError(
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
        raise RuntimeError(
            f"mesh PSK missing at {psk_path}. A relay must be paired before "
            "mesh_manager can start."
        )

    return mesh_id, psk


def _pick_mesh_iface(configured: str | None) -> str | None:
    """Return the wireless interface batman-adv should bind to.

    Priority: explicit config > first wlan* that is not the primary WFB
    adapter. We detect "primary WFB adapter" by checking for monitor
    mode on an RTL8812 family interface.
    """
    if configured:
        return configured

    net_dir = Path("/sys/class/net")
    if not net_dir.is_dir():
        return None

    candidates: list[str] = []
    for p in sorted(net_dir.iterdir()):
        if not p.name.startswith("wlan"):
            continue
        # Skip interfaces already in monitor mode. The primary WFB
        # adapter is typically wlan0 after `iw set wlan0 type monitor`.
        mode_path = p / "type"
        try:
            mode = mode_path.read_text().strip()
        except OSError:
            mode = ""
        # type 1 is "infrastructure-or-mesh" in sysfs, type 803 is monitor.
        if mode == "803":
            continue
        candidates.append(p.name)

    return candidates[0] if candidates else None


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


def _parse_neighbors(text: str) -> list[MeshNeighbor]:
    """Parse `batctl n -H` output.

    Columns are: IF, Neighbor MAC, last-seen, [TQ].
    Format is stable across batman-adv versions from 2020.
    """
    out: list[MeshNeighbor] = []
    now_ms = int(time.time() * 1000)
    for line in text.splitlines():
        parts = line.split()
        if len(parts) < 3:
            continue
        iface = parts[0]
        mac = parts[1]
        # last-seen is "0.550s" form.
        last_seen_s = parts[2].rstrip("s")
        try:
            last_seen_ms = int(float(last_seen_s) * 1000)
        except ValueError:
            last_seen_ms = 0
        tq = 0
        # Some versions include TQ on this row, others only via `o -H`.
        if len(parts) >= 4 and parts[3].isdigit():
            tq = int(parts[3])
        out.append(
            MeshNeighbor(
                mac=mac,
                iface=iface,
                tq=tq,
                last_seen_ms=now_ms - last_seen_ms,
            )
        )
    return out


def _parse_gateways(text: str) -> list[MeshGateway]:
    """Parse `batctl gwl -H` output."""
    out: list[MeshGateway] = []
    for line in text.splitlines():
        parts = line.split()
        if len(parts) < 3:
            continue
        selected = parts[0] == "=>"
        if selected:
            parts = parts[1:]
        if not parts:
            continue
        mac = parts[0]
        # Class column varies. Try to pull "<up>/<down>" pair.
        up_kbps = 0
        down_kbps = 0
        tq = 0
        for tok in parts[1:]:
            if "/" in tok:
                try:
                    up_s, down_s = tok.split("/", 1)
                    up_kbps = int(up_s)
                    down_kbps = int(down_s.rstrip("Mbps").rstrip("kbps") or "0")
                except ValueError:
                    pass
            else:
                # batctl prints TQ as "(240)" in some versions and bare
                # "240" in others. Strip parentheses either way.
                stripped = tok.strip("()")
                if stripped.isdigit():
                    try:
                        tq = int(stripped)
                    except ValueError:
                        pass
        out.append(
            MeshGateway(
                mac=mac,
                class_up_kbps=up_kbps,
                class_down_kbps=down_kbps,
                tq=tq,
                selected=selected,
            )
        )
    return out


async def _poll_once(
    snap: MeshSnapshot,
    prev_neighbors: set[str],
    prev_selected_gw: str | None,
) -> tuple[set[str], str | None]:
    """Refresh snapshot in place, publish events on change."""
    bus = get_mesh_event_bus()
    now_ms = int(time.time() * 1000)

    # Thread-hop subprocess calls so a wedged `batctl` caused by a
    # deadlocked kernel module does not stall the event loop. The
    # kill-safe `_run` bounds the wait to (timeout + 2s).
    rc, out, _e = await asyncio.to_thread(_run, ["batctl", "n", "-H"], 3.0)
    if rc == 0:
        snap.neighbors = _parse_neighbors(out)

    rc, out, _e = await asyncio.to_thread(_run, ["batctl", "gwl", "-H"], 3.0)
    if rc == 0:
        snap.gateways = _parse_gateways(out)
        selected = next((g.mac for g in snap.gateways if g.selected), None)
        snap.selected_gateway = selected

    snap.last_poll_ms = now_ms

    # Neighbor churn events.
    current_neighbors = {n.mac for n in snap.neighbors}
    joined = current_neighbors - prev_neighbors
    left = prev_neighbors - current_neighbors
    for mac in joined:
        await bus.publish(
            MeshEvent(
                kind="neighbor_join",
                timestamp_ms=now_ms,
                payload={"mac": mac},
            )
        )
    for mac in left:
        await bus.publish(
            MeshEvent(
                kind="neighbor_leave",
                timestamp_ms=now_ms,
                payload={"mac": mac},
            )
        )

    # Gateway change event.
    if snap.selected_gateway != prev_selected_gw:
        await bus.publish(
            MeshEvent(
                kind="gateway_changed",
                timestamp_ms=now_ms,
                payload={
                    "previous": prev_selected_gw,
                    "selected": snap.selected_gateway,
                },
            )
        )

    return current_neighbors, snap.selected_gateway


def _write_state_json(snap: MeshSnapshot) -> None:
    """Write a JSON snapshot so the REST layer can serve `/api/v1/mesh/*`
    without calling back into the service (file is the IPC).
    """
    try:
        MESH_STATE_JSON.parent.mkdir(parents=True, exist_ok=True)
        payload = {
            "role": snap.role,
            "bat_iface": snap.bat_iface,
            "mesh_iface": snap.mesh_iface,
            "carrier": snap.carrier,
            "mesh_id": snap.mesh_id,
            "up": snap.up,
            "neighbors": [n.__dict__ for n in snap.neighbors],
            "gateways": [g.__dict__ for g in snap.gateways],
            "selected_gateway": snap.selected_gateway,
            "partition": snap.partition,
            "started_at_ms": snap.started_at_ms,
            "last_poll_ms": snap.last_poll_ms,
        }
        tmp = MESH_STATE_JSON.with_suffix(MESH_STATE_JSON.suffix + ".tmp")
        tmp.write_text(json.dumps(payload), encoding="utf-8")
        os.replace(str(tmp), str(MESH_STATE_JSON))
    except OSError as exc:
        log.debug("mesh_state_write_failed", error=str(exc))


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
        self._snapshot = MeshSnapshot(
            role=self._role,
            bat_iface=self._bat_iface,
            mesh_iface="",
            carrier=self._carrier,
            mesh_id="",
            up=False,
        )
        self._running = False

    @property
    def snapshot(self) -> MeshSnapshot:
        return self._snapshot

    async def setup(self) -> bool:
        """One-shot bringup. Returns True on success."""
        if self._role not in ("relay", "receiver"):
            log.warning("mesh_skip_direct_role")
            return False

        try:
            mesh_id, _psk = _ensure_mesh_identity(self._role, self._config)
        except RuntimeError as exc:
            log.error("mesh_identity_missing", error=str(exc))
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
        # uplink_router owns the real decision and can toggle this via
        # future PUT /mesh/gateway_preference.
        has_uplink = Path("/run/ados/uplink-active").is_file()
        mode = _configure_gateway_mode(
            self._role,
            self._config.ground_station.cloud_uplink,
            has_uplink,
        )
        log.info(
            "mesh_up",
            role=self._role,
            mesh_iface=iface,
            carrier=self._carrier,
            mesh_id=mesh_id,
            gw_mode=mode,
        )

        self._snapshot.mesh_iface = iface
        self._snapshot.mesh_id = mesh_id
        self._snapshot.up = True
        self._snapshot.started_at_ms = int(time.time() * 1000)
        _write_state_json(self._snapshot)
        return True

    async def teardown(self) -> None:
        if self._mesh_iface:
            _run(["batctl", "if", "del", self._mesh_iface], timeout=5.0)
            _run(["iw", "dev", self._mesh_iface, "disconnect"], timeout=5.0)
            _run(["ip", "link", "set", self._mesh_iface, "down"], timeout=5.0)
        _run(["ip", "link", "set", self._bat_iface, "down"], timeout=5.0)
        self._snapshot.up = False
        _write_state_json(self._snapshot)

    async def run_poll_loop(self) -> None:
        self._running = True
        prev_neighbors: set[str] = set()
        prev_selected_gw: str | None = None
        while self._running:
            try:
                prev_neighbors, prev_selected_gw = await _poll_once(
                    self._snapshot, prev_neighbors, prev_selected_gw,
                )
                _write_state_json(self._snapshot)
            except Exception as exc:
                log.debug("mesh_poll_error", error=str(exc))
            await asyncio.sleep(_POLL_INTERVAL_S)

    def stop(self) -> None:
        self._running = False


async def main() -> None:
    config = load_config()
    configure_logging(config.logging.level)
    slog = structlog.get_logger()
    slog.info("mesh_manager_starting")

    manager = MeshManager(config)
    ok = await manager.setup()
    if not ok:
        slog.error("mesh_setup_failed")
        sys.exit(2)

    shutdown = asyncio.Event()
    loop = asyncio.get_event_loop()
    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, shutdown.set)

    poll_task = asyncio.create_task(manager.run_poll_loop(), name="mesh-poll")

    done, _pending = await asyncio.wait(
        [asyncio.create_task(shutdown.wait()), poll_task],
        return_when=asyncio.FIRST_COMPLETED,
    )

    slog.info("mesh_manager_stopping")
    manager.stop()
    poll_task.cancel()
    await asyncio.gather(poll_task, return_exceptions=True)
    await manager.teardown()
    slog.info("mesh_manager_stopped")


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
    sys.exit(0)
