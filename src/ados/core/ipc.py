"""IPC client for the native MAVLink router's state socket.

The state socket (/run/ados/state.sock) streams vehicle state snapshots,
server→clients. This module holds the Python state reader and the state-wire
decoder its callers share.
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
STATE_SOCK = ADOS_RUN_DIR / "state.sock"

# Frame protocol: 4-byte length prefix (network order) + payload
HEADER_SIZE = 4

# State v2 wire: length-prefixed msgpack (the same 4-byte big-endian frame the
# MAVLink socket uses). A state snapshot with the full parameter dict is larger
# than a MAVLink frame, so it gets its own cap.
STATE_MAX_FRAME_SIZE = 1024 * 1024

# A v2 (length-prefixed msgpack) frame begins with a 4-byte big-endian length.
# A state snapshot is always far under 16 MB, so the most-significant length
# byte (the first byte on the wire) is always 0x00 — the discriminant a reader
# uses to tell a v2 frame apart from a v1 JSON object (which starts with '{').
STATE_FRAME_V2_MARKER = b"\x00"


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

    The wire is self-describing: a v2 frame is
    a 4-byte big-endian length prefix + msgpack body whose leading length byte
    is always ``0x00``; a v1 frame is a newline-terminated JSON object whose
    first byte is ``{``. Sniffing that first byte keeps the reader compatible
    with a stray v1 frame even though the producer only ever emits v2.

    Returns the decoded snapshot dict, or None when a well-framed body could
    not be decoded, so the caller can skip that one frame. An out-of-range
    length prefix raises ``ConnectionError``: its body is unread, so the next
    bytes are not a frame boundary and the only recovery is a reconnect.
    Propagates ``asyncio.IncompleteReadError`` / ``OSError`` on EOF or a
    transport error so the caller can reconnect.
    """
    first = await reader.readexactly(1)
    if first == STATE_FRAME_V2_MARKER:
        rest = await reader.readexactly(HEADER_SIZE - 1)
        (length,) = struct.unpack("!I", first + rest)
        if length == 0 or length > STATE_MAX_FRAME_SIZE:
            log.warning("state_ipc_bad_frame_length", length=length)
            raise ConnectionError(f"state frame length {length} out of range")
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


# ── State IPC Client ──────────────────────────────────────────────


# A snapshot older than this is not vehicle state any more. The router
# publishes several times a second, so three seconds of silence means it died
# or restarted; reporting its last frame past that point would describe an FC
# link (connected, maybe armed) that nobody is observing.
STATE_STALE_AFTER_S = 3.0


class StateIPCClient:
    """Connects to state server and receives JSON vehicle state updates."""

    def __init__(
        self,
        sock_path: Path = STATE_SOCK,
        clock: Callable[[], float] = time.monotonic,
    ) -> None:
        self._sock_path = sock_path
        self._reader: asyncio.StreamReader | None = None
        self._writer: asyncio.StreamWriter | None = None
        self._connected = False
        self._state: dict = {}
        self._state_at: float | None = None
        self._on_state: Callable[[dict], None] | None = None
        # The staleness clock. Its own seam so a test can age the snapshot
        # without freezing the event loop's clock, which also reads monotonic.
        self._clock = clock

    @property
    def connected(self) -> bool:
        return self._connected

    @property
    def stale(self) -> bool:
        """True when no snapshot arrived within :data:`STATE_STALE_AFTER_S`."""
        at = self._state_at
        return at is None or (self._clock() - at) > STATE_STALE_AFTER_S

    @property
    def state(self) -> dict:
        """The latest snapshot, or ``{}`` once it is stale."""
        if self.stale:
            return {}
        return self._state

    def _clear_state(self) -> None:
        self._state = {}
        self._state_at = None

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
        self._clear_state()
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
                self._state_at = self._clock()
                if self._on_state:
                    self._on_state(state)
        except (asyncio.IncompleteReadError, ConnectionResetError, OSError):
            pass
        except AttributeError:
            # Reader dropped mid-read during a shutdown race.
            pass
        finally:
            # A closed connection publishes nothing: the last snapshot no
            # longer describes the vehicle.
            self._connected = False
            self._clear_state()
