#!/usr/bin/env python3
"""Side-by-side MAVLink parity harness: Python service vs Rust router.

Runs the Python MAVLink service (``python -m ados.services.mavlink``) and the
Rust router (``ados-mavlink-router``) at the same time, drives both from the
same telemetry source, and compares what each one produces: the 10 Hz vehicle
state snapshot, the frame fan-out, the direct-GCS proxy outputs (TCP / UDP /
WebSocket), and (when the harness is the shared FC) each side's outbound
behaviour toward the FC (companion heartbeat cadence, parameter sweep, stream
interval requests, command pass-through).

The two processes are isolated from each other and from any running agent: each
gets its own ``ADOS_RUN_DIR`` (so the unix sockets do not collide) and its own
set of proxy ports.

Source modes
------------
* ``demo`` (default): each side runs its own synthetic FC (Rust ``--demo`` /
  Python ``--demo``). Hardware-free, runs on macOS. Compares state snapshots,
  fan-out, and proxy outputs. Outbound-to-FC checks are not applicable (there is
  no FC), so they are reported as skipped.
* ``shared``: the harness itself is the FC. It listens on two TCP ports, both
  sides connect to it, and it feeds identical telemetry to both while recording
  each side's outbound frames. This enables the full comparison matrix
  (byte-exact fan-out, outbound heartbeat/param/stream behaviour, command
  pass-through) and still needs no hardware.
* ``sitl``: both sides connect to an external SITL (``--sitl tcp:host:port``).
  Compares state, fan-out, and proxies; outbound-to-FC checks are skipped (the
  harness does not sit between the agent and the SITL).

Exit code is 0 when every non-skipped check passes, 1 otherwise.
"""

from __future__ import annotations

import argparse
import asyncio
import contextlib
import json
import math
import os
import socket
import struct
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path

from pymavlink.dialects.v20 import ardupilotmega as ap

try:
    import websockets  # type: ignore

    _HAVE_WS = True
except Exception:  # noqa: BLE001
    _HAVE_WS = False


# ── Shared demo model (independent reference, mirrors demo.py) ──────────

_CENTER_LAT = 12.9716
_CENTER_LON = 77.5946
_CIRCLE_RADIUS = 0.001
_REVOLUTION_PERIOD = 60.0
_BASE_ALT = 50.0
_ALT_OSCILLATION = 3.0
_BANGALORE_ELEVATION = 920.0
_START_BATTERY = 95
_START_VOLTAGE = 25.2

# The eight telemetry message ids the demo flight emits.
EXPECTED_MSG_IDS = {0, 1, 24, 30, 33, 65, 74, 147}
# The message ids the agent requests from the FC via SET_MESSAGE_INTERVAL.
EXPECTED_STREAM_IDS = {0, 30, 33, 1, 24, 74, 147, 65}


def model_at(t: float) -> dict:
    """The expected flight state at elapsed time ``t`` (seconds)."""
    angle = (2.0 * math.pi * t) / _REVOLUTION_PERIOD
    lat = _CENTER_LAT + _CIRCLE_RADIUS * math.cos(angle)
    lon = _CENTER_LON + _CIRCLE_RADIUS * math.sin(angle)
    alt_rel = _BASE_ALT + _ALT_OSCILLATION * math.sin(t * 0.3)
    heading_rad = angle + math.pi / 2.0
    heading = math.degrees(heading_rad) % 360.0
    vz = _ALT_OSCILLATION * 0.3 * math.cos(t * 0.3)
    remaining = max(0, int(_START_BATTERY - t / 60.0))
    voltage = max(0.0, _START_VOLTAGE * (remaining / 100.0))
    return {
        "lat": lat,
        "lon": lon,
        "alt_rel": alt_rel,
        "alt_msl": alt_rel + _BANGALORE_ELEVATION,
        "heading": heading,
        "vz": vz,
        "climb": -vz,
        "voltage": voltage,
        "remaining": remaining,
    }


# ── Frame helpers ──────────────────────────────────────────────────────


def decode_frames(buf: bytes, parser: ap.MAVLink) -> list:
    """Decode every complete MAVLink message in ``buf`` (raw frame bytes)."""
    msgs = parser.parse_buffer(buf)
    return [m for m in (msgs or []) if m.get_type() != "BAD_DATA"]


def heading_err(a: float, b: float) -> float:
    """Smallest absolute difference between two headings in degrees."""
    d = abs(a - b) % 360.0
    return min(d, 360.0 - d)


# ── Result model ───────────────────────────────────────────────────────


@dataclass
class Check:
    name: str
    status: str  # "pass" | "fail" | "skip"
    detail: str = ""
    metrics: dict = field(default_factory=dict)


# ── Collectors ─────────────────────────────────────────────────────────


async def collect_state(sock_path: Path, stop: float) -> list[tuple[float, dict]]:
    """Read newline-JSON state snapshots from a unix socket until ``stop``."""
    out: list[tuple[float, dict]] = []
    try:
        reader, writer = await _open_unix(sock_path, stop)
    except Exception:  # noqa: BLE001
        return out
    buf = b""
    try:
        while time.monotonic() < stop:
            chunk = await _recv(reader, stop)
            if chunk is None:  # timeout: no data yet, keep waiting
                continue
            if chunk == b"":  # EOF
                break
            buf += chunk
            while b"\n" in buf:
                line, _, buf = buf.partition(b"\n")
                if not line.strip():
                    continue
                with contextlib.suppress(Exception):
                    out.append((time.monotonic(), json.loads(line)))
    finally:
        _close_writer(writer)
    return out


async def collect_ipc_frames(sock_path: Path, stop: float) -> list:
    """Read 4-byte length-prefixed MAVLink frames from a unix socket."""
    msgs: list = []
    try:
        reader, writer = await _open_unix(sock_path, stop)
    except Exception:  # noqa: BLE001
        return msgs
    parser = ap.MAVLink(None)
    buf = b""
    try:
        while time.monotonic() < stop:
            chunk = await _recv(reader, stop)
            if chunk is None:
                continue
            if chunk == b"":
                break
            buf += chunk
            while len(buf) >= 4:
                (length,) = struct.unpack("!I", buf[:4])
                if length == 0 or length > 65536:
                    buf = b""
                    break
                if len(buf) < 4 + length:
                    break
                frame, buf = buf[4 : 4 + length], buf[4 + length :]
                msgs.extend(decode_frames(frame, parser))
    finally:
        _close_writer(writer)
    return msgs


async def _connect_tcp(port: int, stop: float):
    """Connect to a TCP port, retrying until it is listening or ``stop``.

    A proxy can bind late (e.g. the Python service's blocking heartbeat wait
    delays it until the FC link is up), so a single attempt would race and fail.
    """
    while time.monotonic() < stop:
        try:
            return await asyncio.wait_for(
                asyncio.open_connection("127.0.0.1", port), timeout=2.0
            )
        except Exception:  # noqa: BLE001
            await asyncio.sleep(0.1)
    return None


async def collect_tcp_proxy(port: int, stop: float, command: bytes | None) -> list:
    """Read raw MAVLink from a TCP proxy; optionally inject a command frame once."""
    msgs: list = []
    conn = await _connect_tcp(port, stop)
    if conn is None:
        return msgs
    reader, writer = conn
    parser = ap.MAVLink(None)
    # Inject the command partway through the window so frames are already
    # flowing in both directions when it is sent.
    send_at = time.monotonic() + min(1.5, max(0.5, (stop - time.monotonic()) / 3))
    sent = False
    try:
        while time.monotonic() < stop:
            if command and not sent and time.monotonic() >= send_at:
                with contextlib.suppress(Exception):
                    writer.write(command)
                    await writer.drain()
                sent = True
            chunk = await _recv(reader, stop)
            if chunk is None:
                continue
            if chunk == b"":
                break
            msgs.extend(decode_frames(chunk, parser))
    finally:
        _close_writer(writer)
    return msgs


async def collect_udp_proxy(port: int, stop: float, command: bytes | None) -> list:
    """Register with a UDP proxy (send a probe) then read raw MAVLink frames."""
    loop = asyncio.get_running_loop()
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.setblocking(False)
    msgs: list = []
    parser = ap.MAVLink(None)
    probe = ap.MAVLink(None, srcSystem=255, srcComponent=190)
    probe_bytes = bytes(probe.heartbeat_encode(6, 8, 0, 0, 0, 3).pack(probe))
    last_probe = 0.0
    send_at = time.monotonic() + min(1.5, max(0.5, (stop - time.monotonic()) / 3))
    sent = False
    try:
        while time.monotonic() < stop:
            now = time.monotonic()
            # Re-probe periodically until frames arrive (registers us as a peer
            # even if the proxy was not yet forwarding at the first probe).
            if not msgs and now - last_probe > 0.5:
                with contextlib.suppress(Exception):
                    sock.sendto(probe_bytes, ("127.0.0.1", port))
                last_probe = now
            if command and not sent and now >= send_at:
                with contextlib.suppress(Exception):
                    sock.sendto(command, ("127.0.0.1", port))
                sent = True
            try:
                data = await asyncio.wait_for(
                    loop.sock_recv(sock, 4096), timeout=0.3
                )
            except TimeoutError:
                continue
            except Exception:  # noqa: BLE001
                break
            if data:
                msgs.extend(decode_frames(data, parser))
    finally:
        sock.close()
    return msgs


async def collect_ws_proxy(port: int, stop: float) -> list:
    """Read binary MAVLink frames from a WebSocket proxy."""
    msgs: list = []
    if not _HAVE_WS:
        return msgs
    parser = ap.MAVLink(None)
    ws = None
    while ws is None and time.monotonic() < stop:
        try:
            ws = await websockets.connect(
                f"ws://127.0.0.1:{port}", open_timeout=2.0, max_size=None
            )
        except Exception:  # noqa: BLE001
            await asyncio.sleep(0.1)
    if ws is None:
        return msgs
    try:
        async with ws:
            while time.monotonic() < stop:
                try:
                    data = await asyncio.wait_for(
                        ws.recv(), timeout=max(0.05, stop - time.monotonic())
                    )
                except TimeoutError:
                    continue
                except Exception:  # noqa: BLE001
                    break
                if isinstance(data, (bytes, bytearray)):
                    msgs.extend(decode_frames(bytes(data), parser))
    except Exception:  # noqa: BLE001
        return msgs
    return msgs


async def _open_unix(path: Path, stop: float):
    last = None
    while time.monotonic() < stop:
        try:
            return await asyncio.open_unix_connection(str(path))
        except (FileNotFoundError, ConnectionRefusedError, OSError) as exc:
            last = exc
            await asyncio.sleep(0.1)
    raise last or RuntimeError(f"could not connect to {path}")


async def _recv(reader: asyncio.StreamReader, stop: float) -> bytes | None:
    """One short-timeout read. ``None`` = timeout (keep waiting), ``b""`` = EOF.

    Distinguishing the two is the difference between a stream that has not
    produced data yet (common right after the agent connects to the FC) and a
    stream that has closed. Collectors must continue on a timeout and break only
    on EOF, or they quit before any telemetry flows.
    """
    timeout = min(0.5, max(0.02, stop - time.monotonic()))
    try:
        return await asyncio.wait_for(reader.read(4096), timeout=timeout)
    except TimeoutError:
        return None
    except Exception:  # noqa: BLE001
        return b""


def _close_writer(writer: asyncio.StreamWriter) -> None:
    with contextlib.suppress(Exception):
        writer.close()


# ── Shared-FC server (the harness acts as the flight controller) ───────


class FcServer:
    """A TCP MAVLink FC the agents connect to: feeds telemetry, records outbound."""

    def __init__(self) -> None:
        self.outbound: dict[str, list] = {}
        self._servers: dict[str, asyncio.AbstractServer] = {}
        self._writers: dict[str, asyncio.StreamWriter] = {}
        self.ports: dict[str, int] = {}
        # (msgid, payload) of every frame the FC sent, for the byte-exact
        # forwarding check (the agent must re-emit exactly what it received).
        self.sent_keys: set[tuple[int, bytes]] = set()

    async def start(self, labels: list[str]) -> None:
        for label in labels:
            self.outbound[label] = []
            server = await asyncio.start_server(
                self._make_handler(label), "127.0.0.1", 0
            )
            self.ports[label] = server.sockets[0].getsockname()[1]
            self._servers[label] = server

    def _make_handler(self, label: str):
        async def handler(reader: asyncio.StreamReader, writer: asyncio.StreamWriter):
            self._writers[label] = writer
            parser = ap.MAVLink(None)
            try:
                while True:
                    chunk = await reader.read(4096)
                    if not chunk:
                        break
                    self.outbound[label].extend(decode_frames(chunk, parser))
            except Exception:  # noqa: BLE001
                pass

        return handler

    async def feed(self, stop: float) -> None:
        """Stream the demo telemetry to every connected agent at 10 Hz."""
        mav = ap.MAVLink(None, srcSystem=1, srcComponent=1)
        mav.seq = 0
        keyer = ap.MAVLink(None)
        t0 = time.monotonic()
        while time.monotonic() < stop:
            t = time.monotonic() - t0
            for frame in _demo_frames(mav, t):
                for m in decode_frames(frame, keyer):
                    self.sent_keys.add((m.get_msgId(), bytes(m.get_payload() or b"")))
                for w in list(self._writers.values()):
                    with contextlib.suppress(Exception):
                        w.write(frame)
            await asyncio.sleep(0.1)

    async def stop(self) -> None:
        for w in self._writers.values():
            _close_writer(w)
        for s in self._servers.values():
            s.close()
            with contextlib.suppress(Exception):
                await s.wait_closed()


def _demo_frames(mav: ap.MAVLink, t: float) -> list[bytes]:
    """Build the eight demo telemetry frames (shared-source path)."""
    m = model_at(t)
    tb = int(t * 1000.0)
    hdg = int(m["heading"] * 100.0)
    heading_rad = math.radians(m["heading"])
    msgs = [
        mav.heartbeat_encode(2, 3, 209, 5, 4, 3),
        mav.global_position_int_encode(
            tb,
            int(m["lat"] * 1e7),
            int(m["lon"] * 1e7),
            int(m["alt_msl"] * 1000.0),
            int(m["alt_rel"] * 1000.0),
            int(2.0 * math.cos(heading_rad) * 100.0),
            int(2.0 * math.sin(heading_rad) * 100.0),
            int(m["vz"] * 100.0),
            hdg,
        ),
        mav.attitude_encode(tb, 0.0, 0.0, math.radians(m["heading"]), 0.0, 0.0, 0.0),
        mav.sys_status_encode(
            0, 0, 0, 500, int(m["voltage"] * 1000.0), 420, m["remaining"], 0, 0, 0, 0, 0, 0
        ),
        mav.gps_raw_int_encode(
            int(t * 1e6), 3, int(m["lat"] * 1e7), int(m["lon"] * 1e7),
            int(m["alt_msl"] * 1000.0), 120, 180, 200, hdg, 14,
        ),
        mav.vfr_hud_encode(2.1, 2.0, int(m["heading"]), 45, m["alt_rel"], m["climb"]),
        mav.battery_status_encode(
            0, ap.MAV_BATTERY_FUNCTION_ALL, ap.MAV_BATTERY_TYPE_LIPO,
            3250, [0xFFFF] * 10, 420, 0, 0, m["remaining"],
        ),
        mav.rc_channels_encode(tb, 18, *([1500] * 18), 200),
    ]
    return [bytes(msg.pack(mav)) for msg in msgs]


# ── Process management ─────────────────────────────────────────────────


def _free_port() -> int:
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


@dataclass
class Side:
    label: str
    run_dir: Path
    ws_port: int
    tcp_port: int
    udp_ports: list[int]
    proc: object = None
    launch_t: float = 0.0


def _repo_root() -> Path:
    return Path(__file__).resolve().parents[2]


def _find_rust_bin(explicit: str | None) -> Path | None:
    if explicit:
        p = Path(explicit)
        return p if p.exists() else None
    root = _repo_root()
    for cand in (
        root / "crates" / "target" / "debug" / "ados-mavlink-router",
        root / "crates" / "target" / "release" / "ados-mavlink-router",
    ):
        if cand.exists():
            return cand
    return None


def _base_env(side: Side) -> dict:
    env = dict(os.environ)
    env["ADOS_RUN_DIR"] = str(side.run_dir)
    # Force the newline-JSON state wire so the collector reads a stable format.
    env.pop("ADOS_STATE_IPC_MSGPACK", None)
    return env


def launch_python(
    side: Side, python_exe: str, demo: bool, fc: str | None
) -> object:
    import subprocess

    cmd = [
        python_exe,
        "-m",
        "ados.services.mavlink",
        "--ws-port",
        str(side.ws_port),
        "--tcp-port",
        str(side.tcp_port),
        "--udp-ports",
        ",".join(str(p) for p in side.udp_ports),
    ]
    if demo:
        cmd.append("--demo")
    if fc:
        cmd += ["--fc", fc]
    env = _base_env(side)
    env["ADOS_CONFIG"] = "/nonexistent-parity-config"
    side.launch_t = time.monotonic()
    return subprocess.Popen(
        cmd, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
    )


def launch_rust(
    side: Side, rust_bin: Path, demo: bool, fc: str | None
) -> object:
    import subprocess

    env = _base_env(side)
    env["ADOS_MAVLINK_WS_PORT"] = str(side.ws_port)
    env["ADOS_MAVLINK_TCP_PORT"] = str(side.tcp_port)
    env["ADOS_MAVLINK_UDP_PORTS"] = ",".join(str(p) for p in side.udp_ports)
    cmd = [str(rust_bin)]
    if demo:
        cmd.append("--demo")
    if fc:
        # The Rust router reads the FC connection string from config; write a
        # minimal config and point ADOS_CONFIG at it.
        cfg = side.run_dir / "config.yaml"
        cfg.write_text(
            "mavlink:\n"
            f"  serial_port: {fc}\n"
            "  endpoints:\n"
            "    - type: websocket\n"
            f"      port: {side.ws_port}\n"
            "      enabled: true\n"
        )
        env["ADOS_CONFIG"] = str(cfg)
    else:
        env["ADOS_CONFIG"] = "/nonexistent-parity-config"
    side.launch_t = time.monotonic()
    return subprocess.Popen(
        cmd, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
    )


# ── Comparators ────────────────────────────────────────────────────────


def _schema(d: object) -> object:
    """Recursive key/type skeleton of a snapshot, for structural comparison."""
    if isinstance(d, dict):
        return {k: _schema(v) for k, v in sorted(d.items())}
    if isinstance(d, list):
        # Represent a list by the schema of its first element (or "empty").
        return ["empty"] if not d else [_schema(d[0])]
    if isinstance(d, bool):
        return "bool"
    if isinstance(d, int):
        return "number"
    if isinstance(d, float):
        return "number"
    if isinstance(d, str):
        return "str"
    return "null"


# Keys whose presence/absence and value type define the state schema. The two
# producers add slightly different runtime extras, so the schema check focuses
# on the vehicle snapshot core that both must agree on.
_CORE_STATE_KEYS = [
    "mav_type",
    "autopilot",
    "armed",
    "mode",
    "position",
    "velocity",
    "attitude",
    "battery",
    "gps",
    "rc",
    "throttle",
    "last_heartbeat",
    "last_update",
]


def check_state_schema(py: list, rs: list) -> Check:
    py, rs = _live_snaps(py) or py, _live_snaps(rs) or rs
    if not py or not rs:
        return Check("state_schema", "fail", "no state snapshots from one side")
    ps = {k: py[-1][1].get(k) for k in _CORE_STATE_KEYS}
    rsn = {k: rs[-1][1].get(k) for k in _CORE_STATE_KEYS}
    if any(k not in py[-1][1] for k in _CORE_STATE_KEYS):
        return Check("state_schema", "fail", "python snapshot missing core keys")
    if any(k not in rs[-1][1] for k in _CORE_STATE_KEYS):
        return Check("state_schema", "fail", "rust snapshot missing core keys")
    if _schema(ps) != _schema(rsn):
        return Check(
            "state_schema",
            "fail",
            f"schema mismatch py={_schema(ps)} rs={_schema(rsn)}",
        )
    return Check("state_schema", "pass", "core snapshot keys + types match")


# Upper bound for the flight-elapsed fit (covers warmup + collection + startup).
# The demo flight has already been running for the warmup period before the first
# snapshot is collected, so the offset is the flight time at that first snapshot,
# not a small correction. The heading period is 60 s, so a window this size has
# no wrap ambiguity.
_OFFSET_MAX = 20.0
_OFFSET_STEP = 0.02


def _fit_offset(snaps: list) -> float:
    """Flight-elapsed at the first snapshot, fit from the heading signal.

    Each side runs on its own clock and the flight has been running through the
    warmup period, so the model is aligned to each side's stream by finding the
    flight time at the first snapshot that best matches the heading curve. The
    returned value is used as ``elapsed(ts) = offset + (ts - first_ts)``.
    """
    if not snaps:
        return 0.0
    base = snaps[0][0]
    best_off, best_err = 0.0, 1e9
    off = 0.0
    while off <= _OFFSET_MAX:
        err, n = 0.0, 0
        for ts, d in snaps:
            h = d.get("position", {}).get("heading")
            if h is None:
                continue
            err += heading_err(h, model_at(off + (ts - base))["heading"])
            n += 1
        if n:
            err /= n
            if err < best_err:
                best_err, best_off = err, off
        off += _OFFSET_STEP
    return best_off


def _live_snaps(snaps: list) -> list:
    """Drop pre-telemetry snapshots (default state before the FC link is live).

    The state socket publishes at 10 Hz from boot, so in shared/SITL mode the
    first snapshots carry the default all-zero vehicle state until the agent has
    connected to the FC and decoded a heartbeat. Those would not match the
    flight model; keep only snapshots that show live telemetry.
    """
    live = []
    for ts, d in snaps:
        if not d.get("mode"):
            continue
        if not d.get("last_heartbeat"):
            continue
        if d.get("position", {}).get("lat", 0.0) == 0.0:
            continue
        live.append((ts, d))
    return live


def check_state_model(label: str, snaps: list) -> Check:
    """Validate a side's time-varying state against the shared demo model."""
    snaps = _live_snaps(snaps)
    if len(snaps) < 5:
        return Check(f"state_model_{label}", "fail", "too few live state snapshots")
    offset = _fit_offset(snaps)
    base = snaps[0][0]
    tol = {
        "heading": 1.5,
        "lat": 3e-5,
        "lon": 3e-5,
        "alt_rel": 0.4,
        "alt_msl": 0.4,
        "vz": 0.2,
        "climb": 0.2,
        "voltage": 0.3,
    }
    worst: dict[str, float] = {k: 0.0 for k in tol}
    samples = 0
    for ts, d in snaps:
        elapsed = offset + (ts - base)
        if elapsed < 0:
            continue
        mdl = model_at(elapsed)
        pos = d.get("position", {})
        vel = d.get("velocity", {})
        bat = d.get("battery", {})
        pairs = {
            "heading": (pos.get("heading"), mdl["heading"], True),
            "lat": (pos.get("lat"), mdl["lat"], False),
            "lon": (pos.get("lon"), mdl["lon"], False),
            "alt_rel": (pos.get("alt_rel"), mdl["alt_rel"], False),
            "alt_msl": (pos.get("alt_msl"), mdl["alt_msl"], False),
            "vz": (vel.get("vz"), mdl["vz"], False),
            "climb": (vel.get("climb"), mdl["climb"], False),
            "voltage": (bat.get("voltage"), mdl["voltage"], False),
        }
        ok = True
        for k, (got, exp, is_heading) in pairs.items():
            if got is None:
                continue
            diff = heading_err(got, exp) if is_heading else abs(got - exp)
            worst[k] = max(worst[k], diff)
            if diff > tol[k]:
                ok = False
        if ok:
            samples += 1
    failures = {k: round(v, 6) for k, v in worst.items() if v > tol[k]}
    metrics = {"offset_s": round(offset, 3), "worst": {k: round(v, 6) for k, v in worst.items()}}
    if failures:
        return Check(
            f"state_model_{label}",
            "fail",
            f"fields out of tolerance: {failures}",
            metrics,
        )
    return Check(
        f"state_model_{label}",
        "pass",
        f"all time-varying fields track the model (offset {offset:.2f}s)",
        metrics,
    )


def check_state_static(py: list, rs: list) -> Check:
    """Compare the demo's constant state fields across the two sides."""
    py, rs = _live_snaps(py) or py, _live_snaps(rs) or rs
    if not py or not rs:
        return Check("state_static", "fail", "missing state snapshots")
    p, r = py[-1][1], rs[-1][1]
    checks = {
        "mode": (p.get("mode"), r.get("mode")),
        "armed": (p.get("armed"), r.get("armed")),
        "mav_type": (p.get("mav_type"), r.get("mav_type")),
        "autopilot": (p.get("autopilot"), r.get("autopilot")),
        "gps.fix_type": (p.get("gps", {}).get("fix_type"), r.get("gps", {}).get("fix_type")),
        "gps.satellites": (
            p.get("gps", {}).get("satellites"),
            r.get("gps", {}).get("satellites"),
        ),
        "battery.temperature": (
            p.get("battery", {}).get("temperature"),
            r.get("battery", {}).get("temperature"),
        ),
        "battery.current": (
            p.get("battery", {}).get("current"),
            r.get("battery", {}).get("current"),
        ),
        "battery.cell_voltages": (
            p.get("battery", {}).get("cell_voltages"),
            r.get("battery", {}).get("cell_voltages"),
        ),
        "rc.rssi": (p.get("rc", {}).get("rssi"), r.get("rc", {}).get("rssi")),
        "rc.channels": (p.get("rc", {}).get("channels"), r.get("rc", {}).get("channels")),
        "throttle": (p.get("throttle"), r.get("throttle")),
        "fc_connected": (p.get("fc_connected"), r.get("fc_connected")),
    }
    mismatches = {k: checks[k] for k in checks if checks[k][0] != checks[k][1]}
    if mismatches:
        return Check("state_static", "fail", f"mismatched constants: {mismatches}")
    return Check("state_static", "pass", "constant state fields match")


def check_fanout_coverage(py: list, rs: list) -> Check:
    py_ids = {m.get_msgId() for m in py}
    rs_ids = {m.get_msgId() for m in rs}
    if not EXPECTED_MSG_IDS <= py_ids:
        return Check(
            "fanout_coverage",
            "fail",
            f"python fan-out missing ids {sorted(EXPECTED_MSG_IDS - py_ids)}",
        )
    if not EXPECTED_MSG_IDS <= rs_ids:
        return Check(
            "fanout_coverage",
            "fail",
            f"rust fan-out missing ids {sorted(EXPECTED_MSG_IDS - rs_ids)}",
        )
    return Check(
        "fanout_coverage",
        "pass",
        "both fan-outs carry all eight telemetry message ids",
        {"py_ids": sorted(py_ids), "rs_ids": sorted(rs_ids)},
    )


def _first(msgs: list, mtype: str):
    for m in msgs:
        if m.get_type() == mtype:
            return m
    return None


def check_fanout_heartbeat(py: list, rs: list) -> Check:
    ph, rh = _first(py, "HEARTBEAT"), _first(rs, "HEARTBEAT")
    if ph is None or rh is None:
        return Check("fanout_heartbeat", "fail", "no HEARTBEAT in a fan-out")
    fields = ["type", "autopilot", "base_mode", "custom_mode", "system_status"]
    diffs = {f: (getattr(ph, f), getattr(rh, f)) for f in fields if getattr(ph, f) != getattr(rh, f)}
    if diffs:
        return Check("fanout_heartbeat", "fail", f"HEARTBEAT field mismatch: {diffs}")
    return Check(
        "fanout_heartbeat",
        "pass",
        f"HEARTBEAT identical (type={ph.type}, custom_mode={ph.custom_mode})",
    )


def check_proxy(label_proxy: str, py: list, rs: list) -> Check:
    py_ok = len(py) > 0 and {0, 33} <= {m.get_msgId() for m in py}
    rs_ok = len(rs) > 0 and {0, 33} <= {m.get_msgId() for m in rs}
    if py_ok and rs_ok:
        return Check(
            f"proxy_{label_proxy}",
            "pass",
            f"both {label_proxy} proxies stream telemetry "
            f"(py {len(py)} / rs {len(rs)} frames)",
            {"py_frames": len(py), "rs_frames": len(rs)},
        )
    return Check(
        f"proxy_{label_proxy}",
        "fail",
        f"{label_proxy} proxy did not stream telemetry "
        f"(py {len(py)} / rs {len(rs)} frames)",
    )


def check_fanout_exact(sent_keys: set, py: list, rs: list) -> Check:
    """Shared mode: each fan-out frame is byte-exact one the FC actually sent.

    The agent must forward received FC frames verbatim, so every (msgid,
    payload) seen on a fan-out must be one the FC sent. Any frame not in the
    sent set means the bytes were altered in transit.
    """
    if not sent_keys:
        return Check("fanout_exact", "fail", "FC sent no frames to compare against")
    py_keys = {(m.get_msgId(), bytes(m.get_payload() or b"")) for m in py}
    rs_keys = {(m.get_msgId(), bytes(m.get_payload() or b"")) for m in rs}
    py_foreign = py_keys - sent_keys
    rs_foreign = rs_keys - sent_keys
    if py_foreign or rs_foreign:
        return Check(
            "fanout_exact",
            "fail",
            f"fan-out carried frames the FC never sent "
            f"(py {len(py_foreign)}, rs {len(rs_foreign)} altered)",
        )
    if not py_keys or not rs_keys:
        return Check("fanout_exact", "fail", "a fan-out produced no frames")
    return Check(
        "fanout_exact",
        "pass",
        f"every fan-out frame is byte-exact a sent FC frame "
        f"(py {len(py_keys)} / rs {len(rs_keys)} unique payloads)",
    )


def check_outbound(server: FcServer) -> list[Check]:
    """Shared mode: compare each side's outbound-to-FC behaviour."""
    checks: list[Check] = []
    py = server.outbound.get("python", [])
    rs = server.outbound.get("rust", [])

    def heartbeats(msgs: list) -> list:
        return [m for m in msgs if m.get_type() == "HEARTBEAT" and m.get_srcComponent() == 191]

    py_hb, rs_hb = heartbeats(py), heartbeats(rs)
    if py_hb and rs_hb:
        checks.append(
            Check(
                "outbound_heartbeat",
                "pass",
                f"both sides emit a companion heartbeat (py {len(py_hb)} / rs {len(rs_hb)})",
                {"py": len(py_hb), "rs": len(rs_hb)},
            )
        )
    else:
        checks.append(
            Check(
                "outbound_heartbeat",
                "fail",
                f"companion heartbeat missing (py {len(py_hb)} / rs {len(rs_hb)})",
            )
        )

    def has_param_request(msgs: list) -> bool:
        return any(m.get_type() == "PARAM_REQUEST_LIST" for m in msgs)

    if has_param_request(py) and has_param_request(rs):
        checks.append(Check("outbound_param_sweep", "pass", "both sides send PARAM_REQUEST_LIST"))
    else:
        checks.append(
            Check(
                "outbound_param_sweep",
                "fail",
                f"PARAM_REQUEST_LIST missing (py {has_param_request(py)} / rs {has_param_request(rs)})",
            )
        )

    def stream_ids(msgs: list) -> set:
        ids = set()
        for m in msgs:
            if m.get_type() == "COMMAND_LONG" and int(m.command) == ap.MAV_CMD_SET_MESSAGE_INTERVAL:
                ids.add(int(m.param1))
        return ids

    py_streams, rs_streams = stream_ids(py), stream_ids(rs)
    if EXPECTED_STREAM_IDS <= py_streams and EXPECTED_STREAM_IDS <= rs_streams:
        checks.append(
            Check(
                "outbound_stream_requests",
                "pass",
                "both sides request the expected stream intervals",
                {"py": sorted(py_streams), "rs": sorted(rs_streams)},
            )
        )
    else:
        checks.append(
            Check(
                "outbound_stream_requests",
                "fail",
                f"stream-interval requests differ (py {sorted(py_streams)} / rs {sorted(rs_streams)})",
            )
        )
    return checks


def check_command_passthrough(server: FcServer, magic: float) -> Check:
    """Shared mode: the COMMAND_LONG sent to each proxy reached the FC."""

    def saw_cmd(msgs: list) -> bool:
        for m in msgs:
            if m.get_type() == "COMMAND_LONG" and abs(float(m.param1) - magic) < 0.5:
                return True
        return False

    py = saw_cmd(server.outbound.get("python", []))
    rs = saw_cmd(server.outbound.get("rust", []))
    if py and rs:
        return Check("command_passthrough", "pass", "the test command reached the FC on both sides")
    return Check(
        "command_passthrough",
        "fail",
        f"test command did not reach the FC (py {py} / rs {rs})",
    )


def _command_frame(magic: float) -> bytes:
    mav = ap.MAVLink(None, srcSystem=255, srcComponent=190)
    msg = mav.command_long_encode(1, 1, ap.MAV_CMD_DO_SET_MODE, 0, magic, 0, 0, 0, 0, 0, 0)
    return bytes(msg.pack(mav))


# ── Orchestration ──────────────────────────────────────────────────────


async def run_harness(args) -> dict:
    rust_bin = _find_rust_bin(args.rust_bin)
    if rust_bin is None:
        return {
            "ok": False,
            "error": "ados-mavlink-router binary not found. Build it with: "
            "cargo build --manifest-path crates/Cargo.toml -p ados-mavlink-router",
            "checks": [],
        }

    base = Path(args.workdir) if args.workdir else Path("/tmp/ados-mavlink-parity")
    import shutil

    shutil.rmtree(base, ignore_errors=True)
    base.mkdir(parents=True, exist_ok=True)

    py_side = Side(
        "python",
        base / "py",
        ws_port=_free_port(),
        tcp_port=_free_port(),
        udp_ports=[_free_port(), _free_port()],
    )
    rs_side = Side(
        "rust",
        base / "rs",
        ws_port=_free_port(),
        tcp_port=_free_port(),
        udp_ports=[_free_port(), _free_port()],
    )
    py_side.run_dir.mkdir(parents=True, exist_ok=True)
    rs_side.run_dir.mkdir(parents=True, exist_ok=True)

    server: FcServer | None = None
    demo = args.source == "demo"
    py_fc = rs_fc = None
    if args.source == "shared":
        server = FcServer()
        await server.start(["python", "rust"])
        py_fc = f"tcp:127.0.0.1:{server.ports['python']}"
        rs_fc = f"tcp:127.0.0.1:{server.ports['rust']}"
    elif args.source == "sitl":
        if not args.sitl:
            return {"ok": False, "error": "--source sitl requires --sitl tcp:host:port", "checks": []}
        py_fc = rs_fc = args.sitl

    py_proc = launch_python(py_side, args.python, demo, py_fc)
    rs_proc = launch_rust(rs_side, rust_bin, demo, rs_fc)
    py_side.proc, rs_side.proc = py_proc, rs_proc

    checks: list[Check] = []
    try:
        # Give both services a moment to bind their sockets/ports.
        await asyncio.sleep(args.warmup)
        stop = time.monotonic() + args.duration
        magic = 1234.0  # COMMAND_LONG param1 marker for the pass-through check
        cmd_frame = _command_frame(magic)

        feed_task = None
        if server is not None:
            feed_task = asyncio.create_task(server.feed(stop))

        tasks = {
            "py_state": collect_state(py_side.run_dir / "state.sock", stop),
            "rs_state": collect_state(rs_side.run_dir / "state.sock", stop),
            "py_ipc": collect_ipc_frames(py_side.run_dir / "mavlink.sock", stop),
            "rs_ipc": collect_ipc_frames(rs_side.run_dir / "mavlink.sock", stop),
            "py_tcp": collect_tcp_proxy(py_side.tcp_port, stop, cmd_frame),
            "rs_tcp": collect_tcp_proxy(rs_side.tcp_port, stop, cmd_frame),
            "py_udp": collect_udp_proxy(py_side.udp_ports[0], stop, cmd_frame),
            "rs_udp": collect_udp_proxy(rs_side.udp_ports[0], stop, cmd_frame),
            "py_ws": collect_ws_proxy(py_side.ws_port, stop),
            "rs_ws": collect_ws_proxy(rs_side.ws_port, stop),
        }
        results = dict(zip(tasks.keys(), await asyncio.gather(*tasks.values())))
        if feed_task is not None:
            with contextlib.suppress(Exception):
                await feed_task

        # State
        checks.append(check_state_schema(results["py_state"], results["rs_state"]))
        checks.append(check_state_static(results["py_state"], results["rs_state"]))
        checks.append(check_state_model("python", results["py_state"]))
        checks.append(check_state_model("rust", results["rs_state"]))

        # Fan-out
        checks.append(check_fanout_coverage(results["py_ipc"], results["rs_ipc"]))
        checks.append(check_fanout_heartbeat(results["py_ipc"], results["rs_ipc"]))

        # Proxies
        checks.append(check_proxy("tcp", results["py_tcp"], results["rs_tcp"]))
        checks.append(check_proxy("udp", results["py_udp"], results["rs_udp"]))
        if _HAVE_WS:
            checks.append(check_proxy("ws", results["py_ws"], results["rs_ws"]))
        else:
            checks.append(Check("proxy_ws", "skip", "websockets package not installed"))

        # Shared-source-only checks.
        if server is not None:
            checks.append(
                check_fanout_exact(server.sent_keys, results["py_ipc"], results["rs_ipc"])
            )
            checks.extend(check_outbound(server))
            checks.append(check_command_passthrough(server, magic))
        else:
            for name in (
                "fanout_exact",
                "outbound_heartbeat",
                "outbound_param_sweep",
                "outbound_stream_requests",
                "command_passthrough",
            ):
                checks.append(Check(name, "skip", "requires --source shared (or sitl tap)"))
    finally:
        for proc in (py_proc, rs_proc):
            with contextlib.suppress(Exception):
                proc.terminate()
        for proc in (py_proc, rs_proc):
            with contextlib.suppress(Exception):
                proc.wait(timeout=5)
        if server is not None:
            await server.stop()

    n_pass = sum(1 for c in checks if c.status == "pass")
    n_fail = sum(1 for c in checks if c.status == "fail")
    n_skip = sum(1 for c in checks if c.status == "skip")
    return {
        "ok": n_fail == 0 and n_pass > 0,
        "source": args.source,
        "duration_s": args.duration,
        "summary": {"pass": n_pass, "fail": n_fail, "skip": n_skip},
        "checks": [c.__dict__ for c in checks],
    }


def _print_report(report: dict) -> None:
    if "error" in report and report.get("error"):
        print(f"\n  ERROR: {report['error']}\n", file=sys.stderr)
        return
    print()
    print(f"  MAVLink parity  (source={report.get('source')}, {report.get('duration_s')}s)")
    print("  " + "-" * 64)
    for c in report["checks"]:
        mark = {"pass": "PASS", "fail": "FAIL", "skip": "skip"}[c["status"]]
        print(f"  [{mark}] {c['name']:<26} {c['detail']}")
    s = report["summary"]
    print("  " + "-" * 64)
    bar = "GREEN" if report["ok"] else "RED"
    print(f"  {bar}: {s['pass']} passed, {s['fail']} failed, {s['skip']} skipped")
    print()


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="MAVLink Python/Rust parity harness")
    parser.add_argument(
        "--source", choices=["demo", "shared", "sitl"], default="demo",
        help="telemetry source (default: demo)",
    )
    parser.add_argument("--duration", type=float, default=6.0, help="collection window (s)")
    parser.add_argument("--warmup", type=float, default=2.0, help="startup wait before collecting (s)")
    parser.add_argument("--python", default=sys.executable, help="python interpreter for the service")
    parser.add_argument("--rust-bin", default=None, help="path to ados-mavlink-router")
    parser.add_argument("--sitl", default=None, help="external SITL connection (tcp:host:port)")
    parser.add_argument("--workdir", default=None, help="scratch dir for sockets/configs")
    parser.add_argument("--json", default=None, help="write the JSON report to this path ('-' for stdout)")
    args = parser.parse_args(argv)

    report = asyncio.run(run_harness(args))

    if args.json == "-":
        print(json.dumps(report, indent=2))
    elif args.json:
        Path(args.json).write_text(json.dumps(report, indent=2))
    _print_report(report)
    return 0 if report.get("ok") else 1


if __name__ == "__main__":
    raise SystemExit(main())
