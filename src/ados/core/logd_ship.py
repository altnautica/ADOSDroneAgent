"""Non-blocking shipper that mirrors the agent's log records to the local
logging-and-telemetry store's ingest socket.

The store records logs from every process into a durable on-disk database that
survives a reboot and is reachable when the network is down. This module is the
Python producer half: it ships each ``logging`` record the agent emits to the
ingest socket as a length-prefixed msgpack frame, in the exact shape the native
writer decodes.

Design contract (all four properties are load-bearing):

* **Never blocks.** ``emit`` runs on the request path and the asyncio loop, so
  it does no socket I/O inline. It enqueues the record onto a bounded queue and
  returns. A dedicated background thread owns the socket and ships. On a full
  queue the handler drops the lowest-severity records first and always keeps
  ``WARNING`` and above. This is the ``QueueHandler`` / ``QueueListener`` shape.

* **Absent socket is silent.** The store is usually not installed or not yet
  started; the socket is then absent. A connect failure never raises, never
  blocks the agent, and never logs in a way that loops back into the handler.
  The shipper retries on a backoff and keeps draining (and dropping) its queue
  meanwhile, so the queue can never fill from a dead socket.

* **Already-redacted values only.** ``redact_secrets`` runs in the structlog
  processor chain before any record reaches this handler, so the rendered
  message is already sanitized. As belt-and-suspenders for records that reach
  the stdlib logger by another path (a plain ``logging`` call with ``extra=``),
  every structured field this shipper extracts is run through the same
  redaction before it goes on the wire. No secret value is ever shipped.

* **Wire-exact.** The frame is a msgpack map with the short keys
  ``t/v/ts/src/lvl/tgt/msg/f`` and a 4-byte big-endian length prefix, byte
  compatible with the native ingest decoder. ``lvl`` is the lowercase level
  name string (``trace``/``debug``/``info``/``warn``/``error``); ``ts`` is a
  microsecond epoch integer.

The existing stderr/journald sink stays the always-on primary; this shipper is
purely additive enrichment.
"""

from __future__ import annotations

import hashlib
import logging
import queue
import socket
import struct
import sys
import threading
import time
from typing import Any

import msgpack

from ados.core.logging import _REDACT_PREFIX, _SECRET_SUFFIXES
from ados.core.paths import LOGD_INGEST_SOCK

# --- wire constants (must match the native ingest contract) ----------------

#: Wire version stamped on every frame. The reader branches on this when a
#: field's meaning changes; new fields ride the open ``f`` map without a bump.
WIRE_VERSION = 1

#: Maximum framed payload. Records are small; the cap is headroom and a guard
#: against a runaway field map. A frame that would exceed it is dropped.
LOGD_MAX_FRAME = 1024 * 1024

#: The producer source string written into every frame's ``src`` field. Bland
#: and stable so the store can group this producer's rows.
SOURCE = "python-agent"

#: Level-name -> lowercase wire token. The wire carries the lowercase level
#: string (matching the native enum's rename), not the integer. The store maps
#: it to the stored ordinal on its side. ``CRITICAL`` collapses to ``error``
#: (the most severe wire level); a numeric level outside the standard set is
#: clamped to the nearest token by ``_level_token`` (below ``DEBUG`` -> ``trace``).
_LEVEL_TO_TOKEN = {
    logging.DEBUG: "debug",
    logging.INFO: "info",
    logging.WARNING: "warn",
    logging.ERROR: "error",
    logging.CRITICAL: "error",
}

#: The standard ``LogRecord`` attribute names. Anything on a record outside
#: this set is a caller-supplied structured field (a stdlib ``extra=`` value or
#: a field attached by an upstream processor) and is shipped in the ``f`` map.
_STD_RECORD_ATTRS = frozenset(
    {
        "name",
        "msg",
        "args",
        "levelname",
        "levelno",
        "pathname",
        "filename",
        "module",
        "exc_info",
        "exc_text",
        "stack_info",
        "lineno",
        "funcName",
        "created",
        "msecs",
        "relativeCreated",
        "thread",
        "threadName",
        "processName",
        "process",
        "taskName",
        # set by ``logging`` itself, not a user field
        "message",
        "asctime",
    }
)

# --- tunables --------------------------------------------------------------

#: Bounded queue depth between the handler and the shipper thread. Sized so a
#: brief stall (store restarting) buffers a few seconds of records before the
#: drop policy engages, without unbounded memory growth.
_QUEUE_MAXSIZE = 1024

#: Reconnect backoff schedule, seconds. The shipper advances through these on
#: repeated connect failures and resets to the first on a successful connect.
_BACKOFF_SCHEDULE = (1.0, 5.0, 30.0)

#: How long the shipper parks waiting for the next record before re-checking
#: its connection. Bounded so a backoff window cannot exceed the schedule step
#: by more than one poll.
_DRAIN_POLL_S = 0.5

#: Socket send timeout. A write that blocks longer than this is abandoned and
#: the connection is dropped and retried, so a wedged reader can never stall
#: the shipper thread (and therefore can never back the queue up indefinitely).
_SEND_TIMEOUT_S = 2.0

#: Rate limit, seconds, on the single stderr breadcrumb the shipper is allowed
#: to print about a connect failure. Without this a permanently-absent socket
#: would spam stderr once per backoff. Emitted with ``print`` to stderr, never
#: through ``logging``, so it cannot recurse into this handler.
_CONNECT_WARN_INTERVAL_S = 300.0


def _redact_value(key: str, value: str) -> str:
    """Redact one string value for ``key``, byte-identical to the structlog
    ``redact_secrets`` processor.

    Idempotent: an empty value, a non-secret key, or a value already carrying
    the ``redacted:`` sentinel passes through unchanged.
    """
    if not value or value.startswith(_REDACT_PREFIX):
        return value
    kl = key.lower()
    if not any(kl.endswith(s) or kl == s for s in _SECRET_SUFFIXES):
        return value
    head = value[:4]
    digest = hashlib.sha256(value.encode("utf-8", errors="replace")).hexdigest()[:8]
    return f"{_REDACT_PREFIX}{head}...{digest}"


def _level_token(levelno: int) -> str:
    """Map a numeric level to its lowercase wire token, clamping unknown values.

    Values at or above ``ERROR`` map to ``error``; below ``DEBUG`` (custom or
    ``NOTSET``) map to ``trace`` so a verbose custom level is droppable.
    """
    token = _LEVEL_TO_TOKEN.get(levelno)
    if token is not None:
        return token
    if levelno >= logging.ERROR:
        return "error"
    if levelno >= logging.WARNING:
        return "warn"
    if levelno >= logging.INFO:
        return "info"
    if levelno >= logging.DEBUG:
        return "debug"
    return "trace"


def _extract_fields(record: logging.LogRecord) -> dict[str, Any]:
    """Pull caller-supplied structured fields off a record, redacted.

    Captures stdlib ``extra=`` values and any field an upstream processor
    attached as a record attribute. Standard ``LogRecord`` attributes are
    skipped. Every string value is run through the redaction so a field that
    bypassed the structlog chain still cannot leak a secret onto the wire.
    Non-string values pass through; values msgpack cannot represent are
    stringified so a frame never fails to encode.
    """
    fields: dict[str, Any] = {}
    for k, v in record.__dict__.items():
        if k in _STD_RECORD_ATTRS or k.startswith("_"):
            continue
        if isinstance(v, str):
            fields[k] = _redact_value(k, v)
        elif isinstance(v, (int, float, bool)) or v is None:
            fields[k] = v
        else:
            # Best-effort: keep the field but in a form msgpack can carry. The
            # stringified value is run through the same redaction so a non-str
            # object whose ``str()`` reveals a secret under a secret-bearing key
            # cannot leak to the wire.
            fields[k] = _redact_value(k, str(v))
    return fields


def encode_log_frame(record: logging.LogRecord) -> bytes:
    """Encode one ``LogRecord`` as a complete length-prefixed ingest frame.

    The body is a msgpack map: the discriminator ``t='log'`` plus the log
    fields ``v/ts/src/lvl/tgt/msg/f``. ``tgt`` and ``f`` are omitted when empty
    (matching the native ``skip_serializing_if`` so the two encoders agree).
    Returns the 4-byte big-endian length prefix followed by the msgpack body.
    """
    body: dict[str, Any] = {
        "t": "log",
        "v": WIRE_VERSION,
        "ts": int(record.created * 1_000_000),
        "src": SOURCE,
        "lvl": _level_token(record.levelno),
        "msg": record.getMessage(),
    }
    if record.name:
        body["tgt"] = record.name
    fields = _extract_fields(record)
    if fields:
        body["f"] = fields
    payload = msgpack.packb(body, use_bin_type=True)
    return struct.pack("!I", len(payload)) + payload


class LogdQueueHandler(logging.Handler):
    """A ``logging.Handler`` whose ``emit`` is wait-free.

    It enqueues the record onto a bounded queue and returns. The companion
    shipper thread does all socket I/O. On a full queue the handler drops the
    lowest-severity record first and always keeps ``WARNING`` and above, so a
    burst of debug never costs an error line.
    """

    def __init__(self, q: queue.Queue[logging.LogRecord]) -> None:
        super().__init__()
        self._queue = q

    def emit(self, record: logging.LogRecord) -> None:  # noqa: D401
        try:
            self._enqueue(record)
        except Exception:  # pragma: no cover - never let logging raise
            # A handler must never raise into the caller. There is no useful
            # recovery; the record is simply not shipped.
            pass

    def _enqueue(self, record: logging.LogRecord) -> None:
        try:
            self._queue.put_nowait(record)
            return
        except queue.Full:
            pass
        # Queue is full. Keep high-severity records by evicting one droppable
        # record from the head; drop the incoming low-severity record outright.
        if record.levelno >= logging.WARNING:
            self._make_room_for(record)
        # else: incoming low-severity record is shed silently.

    def _make_room_for(self, record: logging.LogRecord) -> None:
        """Evict one low-severity queued record to admit a WARN+ record.

        Best-effort and non-blocking: if the head is itself high-severity it is
        put back and the incoming record is dropped rather than displacing an
        error. Either way the call returns at once and never blocks.
        """
        try:
            victim = self._queue.get_nowait()
        except queue.Empty:
            # Drained between the full check and now; try a plain put.
            try:
                self._queue.put_nowait(record)
            except queue.Full:
                pass
            return
        if victim.levelno >= logging.WARNING:
            # Do not displace a high-severity record. Put it back; drop ours.
            try:
                self._queue.put_nowait(victim)
            except queue.Full:
                pass
            return
        try:
            self._queue.put_nowait(record)
        except queue.Full:
            # Lost the slot to another thread; drop ours rather than spin.
            pass


class LogdShipper(threading.Thread):
    """Background thread that drains the queue and ships frames to the socket.

    Owns the socket connection. Connects lazily and reconnects on a backoff
    after any I/O error; an absent socket is treated as a transient condition,
    never an error. Keeps draining the queue while disconnected (dropping
    droppable records, holding WARN+ briefly) so a dead socket cannot back the
    queue up.
    """

    def __init__(
        self,
        q: queue.Queue[logging.LogRecord],
        socket_path: str = str(LOGD_INGEST_SOCK),
    ) -> None:
        super().__init__(name="logd-shipper", daemon=True)
        self._queue = q
        self._socket_path = socket_path
        self._sock: socket.socket | None = None
        # Named to avoid clashing with ``threading.Thread`` internals
        # (``_stop`` and ``_handle`` are reserved on the base class).
        self._stop_event = threading.Event()
        self._backoff_idx = 0
        self._next_connect_at = 0.0
        self._last_connect_warn = 0.0
        # Set once at least one frame has been shipped successfully; lets a test
        # (or a stats surface) observe liveness without reaching into the socket.
        self._shipped = 0
        self._connected = False

    # -- lifecycle ----------------------------------------------------------

    def stop(self) -> None:
        """Ask the thread to finish draining and exit."""
        self._stop_event.set()

    def run(self) -> None:  # noqa: D401
        while not self._stop_event.is_set():
            try:
                record = self._queue.get(timeout=_DRAIN_POLL_S)
            except queue.Empty:
                continue
            self._ship_record(record)
        self._close()

    # -- shipping -----------------------------------------------------------

    def _ship_record(self, record: logging.LogRecord) -> None:
        """Encode and ship one record; drop it cleanly on any failure.

        A record is never re-queued: re-queuing a frame the socket rejected
        would risk an unbounded loop. The drop policy already protected the
        WARN+ records at the queue boundary.
        """
        try:
            frame = encode_log_frame(record)
        except Exception:  # pragma: no cover - encoding a record must not crash
            return
        if len(frame) > LOGD_MAX_FRAME + 4:
            # Oversized after framing (a pathological field map). Drop it rather
            # than have the reader reject the whole connection.
            return
        if not self._ensure_connected():
            return
        try:
            self._sock.sendall(frame)  # type: ignore[union-attr]
            self._shipped += 1
        except OSError:
            # The reader went away mid-write. Drop this frame, reset the
            # connection, and let the next record trigger a backoff reconnect.
            self._close()

    # -- connection ---------------------------------------------------------

    def _ensure_connected(self) -> bool:
        """Return True if a live connection is available, connecting if due.

        While in a backoff window this returns False immediately (so the caller
        drops the frame) without attempting a connect, keeping the thread
        responsive. A connect failure is silent except for a rate-limited
        stderr breadcrumb.
        """
        if self._sock is not None:
            return True
        now = time.monotonic()
        if now < self._next_connect_at:
            return False
        try:
            s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            s.settimeout(_SEND_TIMEOUT_S)
            s.connect(self._socket_path)
        except OSError as exc:
            self._on_connect_failure(exc)
            return False
        self._sock = s
        self._connected = True
        self._backoff_idx = 0
        self._next_connect_at = 0.0
        return True

    def _on_connect_failure(self, exc: OSError) -> None:
        """Advance the backoff and emit at most one rate-limited breadcrumb.

        An absent socket (the store not installed) is the common case and is
        not an error condition, so the breadcrumb is sparse and goes to stderr
        directly, never through ``logging`` (which would recurse).
        """
        self._connected = False
        delay = _BACKOFF_SCHEDULE[min(self._backoff_idx, len(_BACKOFF_SCHEDULE) - 1)]
        self._next_connect_at = time.monotonic() + delay
        if self._backoff_idx < len(_BACKOFF_SCHEDULE) - 1:
            self._backoff_idx += 1
        now = time.monotonic()
        if now - self._last_connect_warn >= _CONNECT_WARN_INTERVAL_S:
            self._last_connect_warn = now
            # Bland, technical, one-line; stderr only.
            print(
                f"logd shipper: ingest socket unavailable ({exc.__class__.__name__}); "
                f"retrying, falling back to stderr/journald",
                file=sys.stderr,
            )

    def _close(self) -> None:
        if self._sock is not None:
            try:
                self._sock.close()
            except OSError:
                pass
        self._sock = None
        self._connected = False


# --- installation ----------------------------------------------------------

# Module-global so installation is idempotent across the several entry points
# that call ``configure_logging`` in one process.
_handler: LogdQueueHandler | None = None
_shipper: LogdShipper | None = None


def install_logd_handler(
    socket_path: str = str(LOGD_INGEST_SOCK),
    queue_maxsize: int = _QUEUE_MAXSIZE,
) -> LogdQueueHandler | None:
    """Install the non-blocking shipper handler on the root logger.

    Idempotent: a second call in the same process is a no-op and returns the
    existing handler. Safe to call before the store or its socket exist; the
    shipper handles an absent socket silently. Returns the installed handler
    (or the existing one), or ``None`` if installation was suppressed.
    """
    global _handler, _shipper
    if _handler is not None:
        return _handler

    q: queue.Queue[logging.LogRecord] = queue.Queue(maxsize=queue_maxsize)
    handler = LogdQueueHandler(q)
    handler.setLevel(logging.DEBUG)
    shipper = LogdShipper(q, socket_path=socket_path)
    shipper.start()

    logging.getLogger().addHandler(handler)
    _handler = handler
    _shipper = shipper
    return handler


def uninstall_logd_handler() -> None:
    """Remove the handler and stop the shipper thread. Mainly for tests.

    Leaves the root logger without the shipper and joins the shipper thread so
    a subsequent ``install_logd_handler`` starts cleanly.
    """
    global _handler, _shipper
    if _handler is not None:
        logging.getLogger().removeHandler(_handler)
        _handler = None
    if _shipper is not None:
        _shipper.stop()
        _shipper.join(timeout=2.0)
        _shipper = None
