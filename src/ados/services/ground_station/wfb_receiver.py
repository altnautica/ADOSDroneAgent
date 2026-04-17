"""WFB receiver lifecycle.

Runs on nodes with `ground_station.role == "receiver"`. Drives `wfb_rx`
in aggregator mode so it FEC-combines fragments arriving from the
local NIC and from remote relays over batman-adv. Output stream
lands on localhost UDP 5600 so the existing mediamtx-gs pipeline
republishes it as WHEP/RTSP without any change.

Responsibilities:

- Spawn `wfb_rx` with the local drone-facing adapter plus the aggregator
  UDP listen port so remote relays can forward into it.
- Publish `_ados-receiver._tcp` on the mesh interface so relays can
  resolve this node.
- Track per-relay fragment counters + combined FEC-recovered stats.
- Publish `relay_connected` / `relay_disconnected` events when a relay
  appears or drops silent past the grace window.

Systemd unit: `ados-wfb-receiver.service`. Only starts when role
sentinel reads `receiver`.
"""

from __future__ import annotations

import asyncio
import json
import os
import signal
import socket
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path

import structlog

from ados.core.config import load_config
from ados.core.logging import configure_logging, get_logger
from ados.services.wfb.adapter import detect_wfb_adapters, set_monitor_mode
from ados.services.wfb.key_mgr import get_key_paths, key_exists

from .events import MeshEvent, get_mesh_event_bus

log = get_logger("ground_station.wfb_receiver")

RECEIVER_STATE_JSON = Path("/run/ados/wfb-receiver.json")
_RELAY_GRACE_MS = 4000
_POLL_INTERVAL_S = 2.0
_MDNS_SERVICE = "_ados-receiver._tcp"


@dataclass
class RelayStats:
    mac: str
    last_seen_ms: int
    fragments: int = 0


@dataclass
class ReceiverState:
    role: str = "receiver"
    drone_iface: str = ""
    listen_port: int = 5800
    accept_local_nic: bool = True
    mesh_iface: str = "bat0"
    relays: dict[str, RelayStats] = field(default_factory=dict)
    fragments_after_dedup: int = 0
    fec_repaired: int = 0
    output_kbps: int = 0
    up: bool = False


def _write_state(state: ReceiverState) -> None:
    try:
        RECEIVER_STATE_JSON.parent.mkdir(parents=True, exist_ok=True)
        payload = {
            "role": state.role,
            "drone_iface": state.drone_iface,
            "listen_port": state.listen_port,
            "accept_local_nic": state.accept_local_nic,
            "mesh_iface": state.mesh_iface,
            "relays": [
                {
                    "mac": r.mac,
                    "last_seen_ms": r.last_seen_ms,
                    "fragments": r.fragments,
                }
                for r in state.relays.values()
            ],
            "fragments_after_dedup": state.fragments_after_dedup,
            "fec_repaired": state.fec_repaired,
            "output_kbps": state.output_kbps,
            "up": state.up,
        }
        tmp = RECEIVER_STATE_JSON.with_suffix(RECEIVER_STATE_JSON.suffix + ".tmp")
        tmp.write_text(json.dumps(payload), encoding="utf-8")
        os.replace(str(tmp), str(RECEIVER_STATE_JSON))
    except OSError as exc:
        log.debug("receiver_state_write_failed", error=str(exc))


async def _launch_wfb_rx_aggregate(
    drone_iface: str,
    listen_port: int,
    accept_local_nic: bool,
) -> asyncio.subprocess.Process | None:
    """Spawn `wfb_rx -a <port>` with optional local NIC aggregation.

    With `accept_local_nic=True`, wfb_rx also reads fragments from the
    local monitor-mode adapter (the drone-facing RTL8812). With it
    False, the receiver trusts only relay forwards.
    """
    _tx_key, rx_key = get_key_paths()
    cmd = [
        "wfb_rx",
        "-p", "0",
        "-c", "127.0.0.1",
        "-u", "5600",
        "-a", str(listen_port),
        "-K", rx_key,
    ]
    if accept_local_nic:
        cmd.append(drone_iface)

    try:
        proc = await asyncio.create_subprocess_exec(
            *cmd,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        log.info(
            "wfb_receiver_started",
            pid=proc.pid,
            listen_port=listen_port,
            drone_iface=drone_iface if accept_local_nic else "(none)",
        )
        return proc
    except FileNotFoundError:
        log.error("wfb_rx_not_found")
        return None
    except OSError as exc:
        log.error("wfb_receiver_spawn_failed", error=str(exc))
        return None


def _publish_mdns(port: int, mesh_iface: str) -> tuple[object | None, object | None]:
    """Publish `_ados-receiver._tcp` so relays can resolve us on bat0."""
    try:
        from zeroconf import ServiceInfo, Zeroconf
    except ImportError:
        log.warning("zeroconf_not_installed")
        return None, None

    mesh_ip = _iface_ip(mesh_iface)
    if mesh_ip is None:
        log.warning("mdns_publish_no_mesh_ip", iface=mesh_iface)
        return None, None

    zc = Zeroconf()
    hostname = socket.gethostname()
    info = ServiceInfo(
        type_=_MDNS_SERVICE + ".local.",
        name=f"{hostname}.{_MDNS_SERVICE}.local.",
        addresses=[socket.inet_aton(mesh_ip)],
        port=port,
        properties={},
        server=f"{hostname}.local.",
    )
    try:
        zc.register_service(info)
        log.info("mdns_published", service=_MDNS_SERVICE, ip=mesh_ip, port=port)
    except Exception as exc:
        log.error("mdns_publish_failed", error=str(exc))
        zc.close()
        return None, None
    return zc, info


def _iface_ip(iface: str) -> str | None:
    try:
        import fcntl
        import struct
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        try:
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


async def _tail_stats(proc: asyncio.subprocess.Process, state: ReceiverState) -> None:
    """Feed wfb_rx stderr lines into the combined counters."""
    if proc.stderr is None:
        return
    while True:
        try:
            line_bytes = await proc.stderr.readline()
            if not line_bytes:
                break
            line = line_bytes.decode("utf-8", errors="replace").strip()
            # wfb_rx aggregator prints per-session PKT lines plus summary
            # lines. We track n_out (post-dedup) and fec_rec (repaired).
            if "n_out:" in line:
                for tok in line.split():
                    if tok.startswith("n_out:"):
                        try:
                            state.fragments_after_dedup = int(tok.split(":", 1)[1])
                        except ValueError:
                            pass
                    elif tok.startswith("fec_rec:"):
                        try:
                            state.fec_repaired = int(tok.split(":", 1)[1])
                        except ValueError:
                            pass
                    elif tok.startswith("bitrate_kbps:"):
                        try:
                            state.output_kbps = int(tok.split(":", 1)[1])
                        except ValueError:
                            pass
        except Exception as exc:
            log.debug("wfb_receiver_tail_error", error=str(exc))
            break


async def _watch_relay_churn(state: ReceiverState) -> None:
    """Age relays out of the map and publish disconnect events."""
    bus = get_mesh_event_bus()
    while True:
        await asyncio.sleep(_POLL_INTERVAL_S)
        now_ms = int(time.time() * 1000)
        stale = [
            mac for mac, r in state.relays.items()
            if now_ms - r.last_seen_ms > _RELAY_GRACE_MS
        ]
        for mac in stale:
            state.relays.pop(mac, None)
            await bus.publish(
                MeshEvent(
                    kind="relay_disconnected",
                    timestamp_ms=now_ms,
                    payload={"relay_mac": mac},
                )
            )


async def main() -> None:
    config = load_config()
    configure_logging(config.logging.level)
    slog = structlog.get_logger()
    slog.info("wfb_receiver_starting")

    state = ReceiverState(
        listen_port=config.ground_station.wfb_receiver.listen_port,
        accept_local_nic=config.ground_station.wfb_receiver.accept_local_nic,
        mesh_iface=config.ground_station.mesh.bat_iface,
    )

    drone_iface = ""
    if state.accept_local_nic:
        adapters = detect_wfb_adapters()
        compatible = [a for a in adapters if a.is_wfb_compatible and a.supports_monitor]
        if compatible:
            drone_iface = compatible[0].interface_name
            if not set_monitor_mode(drone_iface):
                slog.warning("wfb_receiver_monitor_mode_failed", iface=drone_iface)
                drone_iface = ""
    state.drone_iface = drone_iface

    if not key_exists():
        slog.warning("wfb_receiver_keys_missing")

    zc, svc_info = _publish_mdns(
        state.listen_port,
        state.mesh_iface,
    )

    shutdown = asyncio.Event()
    loop = asyncio.get_event_loop()
    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, shutdown.set)

    proc = await _launch_wfb_rx_aggregate(
        drone_iface,
        state.listen_port,
        state.accept_local_nic and bool(drone_iface),
    )
    if proc is None:
        slog.error("wfb_receiver_aggregator_spawn_failed")
        if zc is not None:
            zc.close()
        sys.exit(2)

    state.up = True
    _write_state(state)

    tail_task = asyncio.create_task(_tail_stats(proc, state), name="rx-tail")
    churn_task = asyncio.create_task(_watch_relay_churn(state), name="rx-churn")

    async def _periodic_write() -> None:
        while not shutdown.is_set():
            _write_state(state)
            try:
                await asyncio.wait_for(shutdown.wait(), timeout=_POLL_INTERVAL_S)
            except asyncio.TimeoutError:
                continue

    writer_task = asyncio.create_task(_periodic_write(), name="rx-writer")

    done, _pending = await asyncio.wait(
        [asyncio.create_task(shutdown.wait()), asyncio.create_task(proc.wait())],
        return_when=asyncio.FIRST_COMPLETED,
    )

    slog.info("wfb_receiver_stopping")
    for t in (tail_task, churn_task, writer_task):
        t.cancel()
    await asyncio.gather(tail_task, churn_task, writer_task, return_exceptions=True)

    if proc.returncode is None:
        proc.terminate()
        try:
            await asyncio.wait_for(proc.wait(), timeout=3.0)
        except asyncio.TimeoutError:
            proc.kill()

    if zc is not None:
        try:
            if svc_info is not None:
                zc.unregister_service(svc_info)
        except Exception:
            pass
        zc.close()

    state.up = False
    _write_state(state)
    slog.info("wfb_receiver_stopped")


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
    sys.exit(0)
