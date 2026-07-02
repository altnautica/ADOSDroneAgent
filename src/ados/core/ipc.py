"""IPC layer for multi-process ADOS agent communication.

Two Unix socket channels:
1. MAVLink socket (/run/ados/mavlink.sock) — binary MAVLink frames, bidirectional
2. State socket (/run/ados/state.sock) — JSON telemetry snapshots, server→clients

The MAVLink service owns both sockets. Other services connect as clients.

Each connected client owns a bounded asyncio.Queue and a dedicated writer task.
Producers enqueue frames synchronously; the writer task drains the queue and
awaits writer.drain() so kernel-buffer backpressure never blocks the producer
or the event loop. A slow client whose queue fills past the high-water mark
gets disconnected rather than allowed to grow unbounded.
"""

from __future__ import annotations

import asyncio
import json
import os
import struct
import time
from collections.abc import Callable
from pathlib import Path

import msgpack as _msgpack
import structlog

from ados.core import paths as _paths
from ados.core.contracts import contract_version

log = structlog.get_logger()

# The state socket speaks v2: a length-prefixed msgpack frame whose body is the
# map {"v": <version>, "s": <state>}. msgpack is a hard dependency (declared in
# pyproject), so a missing import is a loud ImportError at startup, never a
# silent downgrade to the legacy JSON wire. The version integer is sourced from
# the shared contract registry so Rust, Python, and TypeScript cannot drift.
STATE_V2_VERSION = contract_version("state.v2")
assert STATE_V2_VERSION is not None, "state.v2 contract version missing from registry"

# Allow tests and dev rigs to override the runtime root via env var.
# Defaults to the canonical /run/ados/ from `ados.core.paths`.
ADOS_RUN_DIR = Path(os.environ.get("ADOS_RUN_DIR", str(_paths.ADOS_RUN_DIR)))
MAVLINK_SOCK = ADOS_RUN_DIR / "mavlink.sock"
STATE_SOCK = ADOS_RUN_DIR / "state.sock"

# Frame protocol: 4-byte length prefix (network order) + payload
HEADER_SIZE = 4
MAX_FRAME_SIZE = 65536

# Per-client outbound queue depth. Sized for ~1s of headroom at expected rates.
# MAVLink: ~50 Hz aggregate from FC, so 256 frames ≈ 5s of buffering.
# State: 10 Hz, so 32 snapshots ≈ 3s of buffering.
MAVLINK_QUEUE_DEPTH = 256
STATE_QUEUE_DEPTH = 32

# State v2 wire: length-prefixed msgpack (the same 4-byte big-endian frame the
# MAVLink socket uses). A state snapshot with the full parameter dict is larger
# than a MAVLink frame, so it gets its own cap.
STATE_MAX_FRAME_SIZE = 1024 * 1024

# A v2 (length-prefixed msgpack) frame begins with a 4-byte big-endian length.
# A state snapshot is always far under 16 MB, so the most-significant length
# byte (the first byte on the wire) is always 0x00 — the discriminant a reader
# uses to tell a v2 frame apart from a v1 JSON object (which starts with '{').
STATE_FRAME_V2_MARKER = b"\x00"


def _encode_state_frame(state: dict) -> bytes:
    """Encode a state snapshot as a v2 wire frame.

    The v2 body is the msgpack map ``{"v": <version>, "s": <state>}`` (version
    sourced from the ``state.v2`` contract registry), length-prefixed with a
    4-byte big-endian header — the same framing the MAVLink socket uses. This is
    the only format the producer emits; :func:`_encode_state_frame_v1` is kept
    for the migration reader and the interop tests, not for production output.
    """
    body = _msgpack.packb({"v": STATE_V2_VERSION, "s": state}, use_bin_type=True)
    return struct.pack("!I", len(body)) + body


def _encode_state_frame_v1(state: dict) -> bytes:
    """Encode a state snapshot in the legacy v1 wire (newline-terminated JSON).

    Retained so the reader can be exercised against a stray v1 frame and for the
    interop/round-trip tests. The producer always emits v2.
    """
    return json.dumps(state).encode() + b"\n"


def _decode_state_v2_body(body: bytes) -> dict | None:
    """Decode a v2 (length-prefixed msgpack) state body.

    The body is the map ``{"v": <version>, "s": <state>}``. Returns the inner
    state on success, or None (a skippable frame) when the body fails to decode,
    is not the expected shape, or carries a version this build does not
    understand — mirroring the Rust reader, which turns a version mismatch into a
    skipped frame rather than a mis-read.
    """
    try:
        decoded = _msgpack.unpackb(body, raw=False)
    except Exception:  # noqa: BLE001 — tolerate a malformed frame
        return None
    if not isinstance(decoded, dict):
        return None
    version = decoded.get("v")
    if version != STATE_V2_VERSION:
        log.warning("state_ipc_v2_version_skew", got=version, ours=STATE_V2_VERSION)
        return None
    state = decoded.get("s")
    if not isinstance(state, dict):
        return None
    return state


def _decode_state_v1_line(line: bytes) -> dict | None:
    """Decode a v1 (newline-terminated JSON) state line. None on parse failure."""
    try:
        return json.loads(line)
    except (json.JSONDecodeError, ValueError):
        return None


async def _read_state_frame(reader: asyncio.StreamReader) -> dict | None:
    """Read and decode one state snapshot from an asyncio stream.

    The wire is self-describing (see ``StateIPCServer.publish``): a v2 frame is
    a 4-byte big-endian length prefix + msgpack body whose leading length byte
    is always ``0x00``; a v1 frame is a newline-terminated JSON object whose
    first byte is ``{``. Sniffing that first byte keeps the reader compatible
    with a stray v1 frame even though the producer only ever emits v2.

    Returns the decoded snapshot dict, or None when the frame could not be
    decoded (bad length, or an undecodable body) so the caller can skip a
    single malformed frame. Propagates ``asyncio.IncompleteReadError`` /
    ``OSError`` on EOF or a transport error so the caller can reconnect.
    """
    first = await reader.readexactly(1)
    if first == STATE_FRAME_V2_MARKER:
        rest = await reader.readexactly(HEADER_SIZE - 1)
        (length,) = struct.unpack("!I", first + rest)
        if length == 0 or length > STATE_MAX_FRAME_SIZE:
            log.warning("state_ipc_bad_frame_length", length=length)
            return None
        body = await reader.readexactly(length)
        return _decode_state_v2_body(body)
    # v1: newline-terminated JSON; ``first`` is the opening byte.
    rest = await reader.readline()
    return _decode_state_v1_line(first + rest)


def _read_state_frame_from_socket(sock, deadline: float) -> dict | None:
    """Read and decode one state snapshot from a blocking unix socket.

    The synchronous sibling of :func:`_read_state_frame` for a caller that owns
    a plain blocking socket with an overall time budget (see
    ``ados.bootstrap.profile_detect.probe_fc_heartbeat``). All reads are bounded
    by ``deadline`` (a ``time.monotonic()`` value). Same wire sniff and decode
    as the async helper; returns the decoded dict, or None on no data / timeout
    / bad length / an undecodable body.
    """

    def _recv_exact(n: int) -> bytes | None:
        """Read exactly n bytes before the deadline, else None."""
        chunk = bytearray()
        while len(chunk) < n and time.monotonic() < deadline:
            sock.settimeout(max(0.05, deadline - time.monotonic()))
            part = sock.recv(n - len(chunk))
            if not part:
                return None
            chunk.extend(part)
        return bytes(chunk) if len(chunk) == n else None

    first = _recv_exact(1)
    if first == STATE_FRAME_V2_MARKER:
        rest = _recv_exact(HEADER_SIZE - 1)
        if rest is None:
            return None
        (length,) = struct.unpack("!I", first + rest)
        if length == 0 or length > STATE_MAX_FRAME_SIZE:
            return None
        body = _recv_exact(length)
        if body is None:
            return None
        return _decode_state_v2_body(body)
    if not first:
        return None
    # v1: newline-terminated JSON; ``first`` is the opening byte.
    buf = bytearray(first)
    while time.monotonic() < deadline and b"\n" not in buf:
        sock.settimeout(max(0.05, deadline - time.monotonic()))
        part = sock.recv(4096)
        if not part:
            break
        buf.extend(part)
    line, _, _ = bytes(buf).partition(b"\n")
    if not line:
        return None
    return _decode_state_v1_line(line)


def _ensure_run_dir(path: Path | None = None) -> None:
    """Create the directory for the given socket path (or default)."""
    target = path.parent if path is not None else ADOS_RUN_DIR
    target.mkdir(parents=True, exist_ok=True)


class _ClientChannel:
    """Per-client outbound queue + writer task wrapper.

    Owns the StreamWriter and a bounded queue. The writer task is the only
    code path that touches the writer's send buffer, so back-pressure (via
    drain()) stays inside the task and never blocks the producer.
    """

    __slots__ = ("writer", "queue", "task", "_kind", "_peer")

    def __init__(
        self,
        writer: asyncio.StreamWriter,
        max_queue: int,
        kind: str,
    ) -> None:
        self.writer = writer
        self.queue: asyncio.Queue[bytes | None] = asyncio.Queue(maxsize=max_queue)
        self.task: asyncio.Task | None = None
        self._kind = kind
        self._peer = writer.get_extra_info("peername") or "unknown"

    def start(self) -> None:
        self.task = asyncio.create_task(
            self._writer_loop(), name=f"ipc-{self._kind}-writer"
        )

    async def _writer_loop(self) -> None:
        """Drain the queue, write to socket, await drain. Sentinel None ends."""
        try:
            while True:
                item = await self.queue.get()
                if item is None:
                    return
                self.writer.write(item)
                await self.writer.drain()
        except (ConnectionResetError, BrokenPipeError, OSError) as exc:
            log.debug(
                "ipc_writer_exit",
                kind=self._kind,
                peer=str(self._peer),
                reason=type(exc).__name__,
            )
        except asyncio.CancelledError:
            raise

    def enqueue(self, payload: bytes) -> bool:
        """Try to enqueue. Returns False if queue is full (caller disconnects)."""
        try:
            self.queue.put_nowait(payload)
            return True
        except asyncio.QueueFull:
            return False

    async def close(self) -> None:
        """Stop writer task and close the socket."""
        try:
            self.queue.put_nowait(None)
        except asyncio.QueueFull:
            pass
        if self.task and not self.task.done():
            try:
                await asyncio.wait_for(self.task, timeout=1.0)
            except (TimeoutError, asyncio.CancelledError):
                self.task.cancel()
        try:
            self.writer.close()
        except Exception:
            pass


# ── MAVLink IPC Server (runs in ados-mavlink service) ──────────────


class MavlinkIPCServer:
    """Unix socket server that broadcasts MAVLink frames to all connected clients.

    The MAVLink service writes FC data here. Other services (API, cloud)
    connect and receive a copy of every frame. Clients can also send frames back
    (commands to FC).
    """

    def __init__(
        self,
        sock_path: Path = MAVLINK_SOCK,
        queue_depth: int = MAVLINK_QUEUE_DEPTH,
    ) -> None:
        self._sock_path = sock_path
        self._clients: set[_ClientChannel] = set()
        self._server: asyncio.AbstractServer | None = None
        self._on_client_data: Callable[[bytes], None] | None = None
        self._queue_depth = queue_depth

    def set_command_handler(self, handler: Callable[[bytes], None]) -> None:
        """Register callback for data received from clients (commands to FC)."""
        self._on_client_data = handler

    @property
    def client_count(self) -> int:
        return len(self._clients)

    async def start(self) -> None:
        """Start listening on Unix socket."""
        _ensure_run_dir(self._sock_path)
        # Remove stale socket
        if self._sock_path.exists():
            self._sock_path.unlink()

        self._server = await asyncio.start_unix_server(
            self._handle_client, path=str(self._sock_path)
        )
        # Allow all users to connect
        os.chmod(str(self._sock_path), 0o666)
        log.info("mavlink_ipc_started", path=str(self._sock_path))

    async def stop(self) -> None:
        """Stop server and disconnect all clients."""
        if self._server:
            self._server.close()
            await self._server.wait_closed()
        clients = list(self._clients)
        self._clients.clear()
        if clients:
            await asyncio.gather(
                *(c.close() for c in clients), return_exceptions=True
            )
        if self._sock_path.exists():
            self._sock_path.unlink()
        log.info("mavlink_ipc_stopped")

    def broadcast(self, data: bytes) -> None:
        """Send MAVLink frame to all connected clients (non-blocking)."""
        if not self._clients:
            return
        frame = struct.pack("!I", len(data)) + data
        slow: list[_ClientChannel] = []
        for client in self._clients:
            if not client.enqueue(frame):
                slow.append(client)
        for client in slow:
            log.warning(
                "mavlink_ipc_slow_client_dropped",
                queue_depth=self._queue_depth,
            )
            self._clients.discard(client)
            asyncio.create_task(client.close(), name="ipc-close-slow")

    async def _handle_client(
        self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter
    ) -> None:
        """Handle a connected IPC client."""
        client = _ClientChannel(writer, self._queue_depth, "mavlink")
        client.start()
        self._clients.add(client)
        peer = writer.get_extra_info("peername") or "unknown"
        log.debug("mavlink_ipc_client_connected", peer=peer, total=len(self._clients))
        try:
            while True:
                header = await reader.readexactly(HEADER_SIZE)
                (length,) = struct.unpack("!I", header)
                if length > MAX_FRAME_SIZE:
                    log.warning("mavlink_ipc_oversized_frame", length=length)
                    break
                data = await reader.readexactly(length)
                # Client sending data back = command to FC
                if self._on_client_data:
                    self._on_client_data(data)
        except (asyncio.IncompleteReadError, ConnectionResetError, OSError):
            pass
        finally:
            self._clients.discard(client)
            await client.close()
            log.debug("mavlink_ipc_client_disconnected", total=len(self._clients))


# ── MAVLink IPC Client (runs in other services) ───────────────────


class MavlinkIPCClient:
    """Connects to the MAVLink IPC server to receive/send MAVLink frames."""

    def __init__(self, sock_path: Path = MAVLINK_SOCK) -> None:
        self._sock_path = sock_path
        self._reader: asyncio.StreamReader | None = None
        self._writer: asyncio.StreamWriter | None = None
        self._connected = False
        self._on_data: Callable[[bytes], None] | None = None

    @property
    def connected(self) -> bool:
        return self._connected

    def set_data_handler(self, handler: Callable[[bytes], None]) -> None:
        """Register callback for incoming MAVLink frames from FC."""
        self._on_data = handler

    async def connect(self, retries: int = 10, delay: float = 1.0) -> None:
        """Connect to MAVLink IPC server with retry."""
        for attempt in range(retries):
            try:
                self._reader, self._writer = await asyncio.open_unix_connection(
                    str(self._sock_path)
                )
                self._connected = True
                log.info("mavlink_ipc_connected", path=str(self._sock_path))
                return
            except (FileNotFoundError, ConnectionRefusedError, OSError) as exc:
                if attempt < retries - 1:
                    log.debug(
                        "mavlink_ipc_retry",
                        attempt=attempt + 1,
                        error=str(exc),
                    )
                    await asyncio.sleep(delay)
                else:
                    raise ConnectionError(
                        f"Failed to connect to {self._sock_path} after {retries} attempts"
                    ) from exc

    async def disconnect(self) -> None:
        """Disconnect from server."""
        self._connected = False
        if self._writer:
            self._writer.close()
            self._writer = None
        self._reader = None

    def send(self, data: bytes) -> None:
        """Send MAVLink frame (command) to the server.

        Synchronous so the paho and WebSocket uplink callers can call it
        directly. The write is followed by a scheduled drain so kernel
        send-buffer backpressure is honored instead of letting the
        transport buffer grow unbounded. When the underlying buffer is
        already past its high-water mark the frame is still queued (a
        command must not be silently dropped) but the saturation is
        logged so a stalled IPC consumer is visible.
        """
        if self._writer and self._connected:
            frame = struct.pack("!I", len(data)) + data
            try:
                transport = self._writer.transport
                if transport is not None:
                    buffered = transport.get_write_buffer_size()
                    high, _low = transport.get_write_buffer_limits()
                    if high and buffered >= high:
                        # Past the high-water mark: the consumer is
                        # draining too slowly. Surface it rather than
                        # dropping the command silently. The frame is
                        # still queued below; drain() then applies
                        # backpressure on the next loop turn.
                        log.warning(
                            "mavlink_ipc_send_backpressure",
                            buffered=buffered,
                            high_water=high,
                        )
                self._writer.write(frame)
                # Schedule a drain so the producer awaits kernel-buffer
                # backpressure on the next loop turn without blocking
                # this synchronous caller.
                try:
                    loop = asyncio.get_running_loop()
                    loop.create_task(self._drain())
                except RuntimeError:
                    # No running loop (e.g. a paho callback thread): the
                    # write is already queued on the transport and will
                    # flush on the loop's next pass.
                    pass
            except (ConnectionResetError, BrokenPipeError, OSError):
                self._connected = False

    async def _drain(self) -> None:
        """Await the writer's send buffer so backpressure is honored."""
        writer = self._writer
        if writer is None:
            return
        try:
            await writer.drain()
        except (ConnectionResetError, BrokenPipeError, OSError):
            self._connected = False

    async def read_loop(self) -> None:
        """Read frames from server and dispatch to handler. Runs until disconnect."""
        if not self._reader:
            raise RuntimeError("Not connected")
        try:
            while self._connected:
                # self._reader can become None during a shutdown race
                # (disconnect() sets it to None while this loop is between
                # reads). Snapshot locally and guard.
                reader = self._reader
                if reader is None:
                    break
                header = await reader.readexactly(HEADER_SIZE)
                (length,) = struct.unpack("!I", header)
                if length > MAX_FRAME_SIZE:
                    log.warning("mavlink_ipc_oversized_frame", length=length)
                    break
                reader = self._reader
                if reader is None:
                    break
                data = await reader.readexactly(length)
                if self._on_data:
                    self._on_data(data)
        except (asyncio.IncompleteReadError, ConnectionResetError, OSError):
            self._connected = False
        except AttributeError:
            # Reader dropped mid-read during shutdown race
            self._connected = False


# ── State IPC Server (runs in ados-mavlink, broadcasts VehicleState) ──


class StateIPCServer:
    """Broadcasts JSON vehicle state snapshots to connected clients at 10Hz."""

    def __init__(
        self,
        sock_path: Path = STATE_SOCK,
        queue_depth: int = STATE_QUEUE_DEPTH,
    ) -> None:
        self._sock_path = sock_path
        self._clients: set[_ClientChannel] = set()
        self._server: asyncio.AbstractServer | None = None
        self._last_state: dict | None = None
        self._queue_depth = queue_depth

    @property
    def client_count(self) -> int:
        return len(self._clients)

    async def start(self) -> None:
        """Start state broadcast server."""
        _ensure_run_dir(self._sock_path)
        if self._sock_path.exists():
            self._sock_path.unlink()

        self._server = await asyncio.start_unix_server(
            self._handle_client, path=str(self._sock_path)
        )
        os.chmod(str(self._sock_path), 0o666)
        log.info("state_ipc_started", path=str(self._sock_path))

    async def stop(self) -> None:
        if self._server:
            self._server.close()
            await self._server.wait_closed()
        clients = list(self._clients)
        self._clients.clear()
        if clients:
            await asyncio.gather(
                *(c.close() for c in clients), return_exceptions=True
            )
        if self._sock_path.exists():
            self._sock_path.unlink()

    def publish(self, state: dict) -> None:
        """Broadcast state snapshot to all clients (non-blocking).

        The wire is v2: a length-prefixed msgpack ``{"v", "s"}`` frame (~3-5x
        cheaper to serialize on Pi-class hardware than JSON). The reader
        (``StateIPCClient.read_loop``) is self-describing and still accepts a
        stray v1 frame per frame, but the producer only ever emits v2.
        """
        self._last_state = state
        if not self._clients:
            return
        payload = _encode_state_frame(state)
        slow: list[_ClientChannel] = []
        for client in self._clients:
            if not client.enqueue(payload):
                slow.append(client)
        for client in slow:
            log.warning(
                "state_ipc_slow_client_dropped",
                queue_depth=self._queue_depth,
            )
            self._clients.discard(client)
            asyncio.create_task(client.close(), name="ipc-close-slow")

    async def _handle_client(
        self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter
    ) -> None:
        """New client connected. Send last known state immediately, then keep alive."""
        client = _ClientChannel(writer, self._queue_depth, "state")
        client.start()
        self._clients.add(client)
        # Send current state immediately so client doesn't wait for next publish
        if self._last_state is not None:
            initial = _encode_state_frame(self._last_state)
            client.enqueue(initial)
        # Keep connection alive until client disconnects
        try:
            await reader.read(1)  # blocks until EOF (client disconnect)
        except (ConnectionResetError, OSError):
            pass
        finally:
            self._clients.discard(client)
            await client.close()


# ── State IPC Client (runs in other services, reads VehicleState) ──


class StateIPCClient:
    """Connects to state server and receives JSON vehicle state updates."""

    def __init__(self, sock_path: Path = STATE_SOCK) -> None:
        self._sock_path = sock_path
        self._reader: asyncio.StreamReader | None = None
        self._writer: asyncio.StreamWriter | None = None
        self._connected = False
        self._state: dict = {}
        self._on_state: Callable[[dict], None] | None = None

    @property
    def connected(self) -> bool:
        return self._connected

    @property
    def state(self) -> dict:
        return self._state

    def set_state_handler(self, handler: Callable[[dict], None]) -> None:
        """Register callback for state updates."""
        self._on_state = handler

    async def connect(self, retries: int = 10, delay: float = 1.0) -> None:
        """Connect to state server with retry."""
        for attempt in range(retries):
            try:
                self._reader, self._writer = await asyncio.open_unix_connection(
                    str(self._sock_path)
                )
                self._connected = True
                log.info("state_ipc_connected", path=str(self._sock_path))
                return
            except (FileNotFoundError, ConnectionRefusedError, OSError) as exc:
                if attempt < retries - 1:
                    await asyncio.sleep(delay)
                else:
                    raise ConnectionError(
                        f"Failed to connect to {self._sock_path} after {retries} attempts"
                    ) from exc

    async def disconnect(self) -> None:
        self._connected = False
        if self._writer:
            self._writer.close()
            self._writer = None
        # Null the reader so an in-flight read_loop sees the shutdown on its
        # next iteration (it snapshots self._reader at the top of each loop).
        self._reader = None

    async def read_loop(self) -> None:
        """Read state updates and dispatch to the handler until disconnect.

        Each frame is decoded by :func:`_read_state_frame`, which auto-detects
        the wire format (v1 JSON / v2 length-prefixed msgpack) per frame, so a
        stray v1 frame is still read correctly even though the producer only
        ever emits v2.
        """
        if not self._reader:
            raise RuntimeError("Not connected")
        try:
            while self._connected:
                # Snapshot the reader: disconnect() can null it mid-read.
                reader = self._reader
                if reader is None:
                    break
                state = await _read_state_frame(reader)
                if state is None:
                    # Malformed / undecodable frame — skip it and keep reading.
                    continue
                self._state = state
                if self._on_state:
                    self._on_state(state)
        except (asyncio.IncompleteReadError, ConnectionResetError, OSError):
            pass
        except AttributeError:
            # Reader dropped mid-read during a shutdown race.
            pass
        finally:
            self._connected = False
