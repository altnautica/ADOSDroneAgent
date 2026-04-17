"""WFB relay lifecycle.

Runs on nodes with `ground_station.role == "relay"`. Drives `wfb_rx -f`
to forward WFB video fragments over batman-adv to the receiver. The
receiver is discovered via mDNS `_ados-receiver._tcp` on the mesh
interface (`bat0` by default).

Lifecycle:

1. Detect a compatible RTL8812 adapter in monitor mode (same as direct
   mode `wfb_rx`). This is the drone-facing adapter, not the mesh
   adapter.
2. Resolve the current receiver address via mDNS on `bat0`.
3. Launch `wfb_rx -f <receiver_ip>:<port>` and tail its stats.
4. Publish fragment counters + receiver reachability on the state file.
5. On receiver loss (mDNS timeout), enter `receiver_unreachable` state
   and publish a `receiver_unreachable` event so the OLED can prompt
   the operator to go direct or wait.

Systemd unit: `ados-wfb-relay.service`. Only starts when role sentinel
reads `relay`.
"""

from __future__ import annotations

import asyncio
import json
import os
import signal
import sys
import time
from dataclasses import dataclass
from pathlib import Path

import structlog

from ados.core.config import load_config
from ados.core.logging import configure_logging, get_logger
from ados.services.wfb.adapter import detect_wfb_adapters, set_monitor_mode
from ados.services.wfb.key_mgr import get_key_paths, key_exists

from .events import MeshEvent, get_mesh_event_bus

log = get_logger("ground_station.wfb_relay")

RELAY_STATE_JSON = Path("/run/ados/wfb-relay.json")
_RECEIVER_MDNS_SERVICE = "_ados-receiver._tcp"
_RECEIVER_LOST_GRACE_S = 15.0
_POLL_INTERVAL_S = 2.0


@dataclass
class RelayState:
    role: str = "relay"
    drone_iface: str = ""
    receiver_ip: str | None = None
    receiver_port: int = 5800
    receiver_last_seen_ms: int = 0
    fragments_seen: int = 0
    fragments_forwarded: int = 0
    up: bool = False
    mesh_iface: str = "bat0"


def _write_state(state: RelayState) -> None:
    try:
        RELAY_STATE_JSON.parent.mkdir(parents=True, exist_ok=True)
        payload = {
            "role": state.role,
            "drone_iface": state.drone_iface,
            "receiver_ip": state.receiver_ip,
            "receiver_port": state.receiver_port,
            "receiver_last_seen_ms": state.receiver_last_seen_ms,
            "fragments_seen": state.fragments_seen,
            "fragments_forwarded": state.fragments_forwarded,
            "up": state.up,
            "mesh_iface": state.mesh_iface,
        }
        tmp = RELAY_STATE_JSON.with_suffix(RELAY_STATE_JSON.suffix + ".tmp")
        tmp.write_text(json.dumps(payload), encoding="utf-8")
        os.replace(str(tmp), str(RELAY_STATE_JSON))
    except OSError as exc:
        log.debug("relay_state_write_failed", error=str(exc))


async def _resolve_receiver(
    service_type: str,
    mesh_iface: str,
    timeout: float = 3.0,
) -> tuple[str, int] | None:
    """Resolve the receiver via zeroconf, scoped to `mesh_iface` when possible.

    zeroconf does not bind to an interface name directly. Instead we
    filter resolved records by the `mesh_iface` IP (the address of
    `bat0`) so we stay on the mesh and never see a receiver on the
    shared LAN.
    """
    try:
        from zeroconf import ServiceBrowser, Zeroconf
    except ImportError:
        log.error("zeroconf_not_installed")
        return None

    # Get the local bat0 IP so we can filter to that subnet.
    bat_ip = _iface_ip(mesh_iface)

    service_fullname = service_type.rstrip(".") + ".local."
    found: asyncio.Future[tuple[str, int]] = asyncio.get_event_loop().create_future()

    class _Listener:
        def add_service(self, zc: Zeroconf, stype: str, name: str) -> None:
            info = zc.get_service_info(stype, name, timeout=1500)
            if info is None or not info.addresses:
                return
            for addr_bytes in info.addresses:
                try:
                    import socket
                    ip = socket.inet_ntoa(addr_bytes)
                except OSError:
                    continue
                if bat_ip and not _same_subnet(ip, bat_ip):
                    continue
                if not found.done():
                    found.set_result((ip, info.port))
                return

        def remove_service(self, *args, **kwargs) -> None:
            pass

        def update_service(self, *args, **kwargs) -> None:
            pass

    zc = Zeroconf()
    try:
        ServiceBrowser(zc, service_fullname, _Listener())
        try:
            return await asyncio.wait_for(found, timeout=timeout)
        except asyncio.TimeoutError:
            return None
    finally:
        zc.close()


def _iface_ip(iface: str) -> str | None:
    try:
        import socket, fcntl, struct
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        try:
            # SIOCGIFADDR = 0x8915
            packed = fcntl.ioctl(
                s.fileno(),
                0x8915,
                struct.pack("256s", iface.encode()[:15]),
            )
            return socket.inet_ntoa(packed[20:24])
        finally:
            s.close()
    except OSError:
        return None


def _same_subnet(a: str, b: str, mask_prefix: int = 24) -> bool:
    try:
        import ipaddress
        net = ipaddress.ip_network(f"{b}/{mask_prefix}", strict=False)
        return ipaddress.ip_address(a) in net
    except ValueError:
        return False


async def _launch_wfb_rx_forward(
    drone_iface: str,
    receiver_ip: str,
    receiver_port: int,
) -> asyncio.subprocess.Process | None:
    """Spawn `wfb_rx -f <ip>:<port>` on the drone-facing adapter."""
    _tx_key, rx_key = get_key_paths()
    cmd = [
        "wfb_rx",
        "-p", "0",
        "-f", f"{receiver_ip}:{receiver_port}",
        "-K", rx_key,
        drone_iface,
    ]
    try:
        proc = await asyncio.create_subprocess_exec(
            *cmd,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        log.info(
            "wfb_relay_started",
            pid=proc.pid,
            drone_iface=drone_iface,
            receiver=f"{receiver_ip}:{receiver_port}",
        )
        return proc
    except FileNotFoundError:
        log.error("wfb_rx_not_found")
        return None
    except OSError as exc:
        log.error("wfb_relay_spawn_failed", error=str(exc))
        return None


async def _tail_stats(
    proc: asyncio.subprocess.Process,
    state: RelayState,
) -> None:
    """Feed wfb_rx stderr lines into the fragment counter."""
    if proc.stderr is None:
        return
    while True:
        try:
            line_bytes = await proc.stderr.readline()
            if not line_bytes:
                break
            line = line_bytes.decode("utf-8", errors="replace").strip()
            # wfb_rx prints "PKT n_all:x n_out:y" style stats.
            if "PKT" in line:
                for tok in line.split():
                    if tok.startswith("n_all:"):
                        try:
                            state.fragments_seen = int(tok.split(":", 1)[1])
                        except ValueError:
                            pass
                    elif tok.startswith("n_out:"):
                        try:
                            state.fragments_forwarded = int(tok.split(":", 1)[1])
                        except ValueError:
                            pass
        except Exception as exc:
            log.debug("wfb_relay_tail_error", error=str(exc))
            break


async def main() -> None:
    config = load_config()
    configure_logging(config.logging.level)
    slog = structlog.get_logger()
    slog.info("wfb_relay_starting")

    state = RelayState(
        mesh_iface=config.ground_station.mesh.bat_iface,
        receiver_port=config.ground_station.wfb_relay.receiver_port,
    )

    # Detect drone-facing WFB adapter.
    adapters = detect_wfb_adapters()
    compatible = [a for a in adapters if a.is_wfb_compatible and a.supports_monitor]
    if not compatible:
        slog.error("wfb_relay_no_adapter")
        sys.exit(2)
    drone_iface = compatible[0].interface_name
    if not set_monitor_mode(drone_iface):
        slog.error("wfb_relay_monitor_mode_failed", iface=drone_iface)
        sys.exit(3)
    state.drone_iface = drone_iface

    if not key_exists():
        slog.warning("wfb_relay_keys_missing")

    bus = get_mesh_event_bus()
    shutdown = asyncio.Event()
    loop = asyncio.get_event_loop()
    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, shutdown.set)

    proc: asyncio.subprocess.Process | None = None
    current_receiver: tuple[str, int] | None = None

    try:
        while not shutdown.is_set():
            # Resolve or re-resolve the receiver.
            resolved = await _resolve_receiver(
                config.ground_station.wfb_relay.receiver_mdns_service,
                config.ground_station.mesh.bat_iface,
            )

            now_ms = int(time.time() * 1000)

            if resolved:
                state.receiver_last_seen_ms = now_ms
                if resolved != current_receiver:
                    # Receiver changed: tear down the old forwarder.
                    if proc is not None and proc.returncode is None:
                        proc.terminate()
                        try:
                            await asyncio.wait_for(proc.wait(), timeout=3.0)
                        except asyncio.TimeoutError:
                            proc.kill()
                    current_receiver = resolved
                    state.receiver_ip = resolved[0]
                    state.receiver_port = resolved[1]
                    proc = await _launch_wfb_rx_forward(
                        drone_iface, resolved[0], resolved[1],
                    )
                    if proc is not None:
                        state.up = True
                        asyncio.create_task(_tail_stats(proc, state))
            elif state.receiver_last_seen_ms > 0:
                stale_ms = now_ms - state.receiver_last_seen_ms
                if stale_ms > _RECEIVER_LOST_GRACE_S * 1000:
                    if state.up:
                        state.up = False
                        await bus.publish(
                            MeshEvent(
                                kind="receiver_unreachable",
                                timestamp_ms=now_ms,
                                payload={
                                    "last_receiver": state.receiver_ip,
                                    "stale_ms": stale_ms,
                                },
                            )
                        )
                        if proc is not None and proc.returncode is None:
                            proc.terminate()

            _write_state(state)

            try:
                await asyncio.wait_for(shutdown.wait(), timeout=_POLL_INTERVAL_S)
            except asyncio.TimeoutError:
                continue
    finally:
        if proc is not None and proc.returncode is None:
            proc.terminate()
            try:
                await asyncio.wait_for(proc.wait(), timeout=3.0)
            except asyncio.TimeoutError:
                proc.kill()
        state.up = False
        _write_state(state)
        slog.info("wfb_relay_stopped")


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
    sys.exit(0)
