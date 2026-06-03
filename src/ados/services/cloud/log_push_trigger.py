"""Thin trigger for an explicit, operator-initiated cloud export of a log window.

This module signals *intent* and reports the *outcome*; it does no exporting,
uploading, or marking itself. Those steps run in the long-running cloud service,
which owns the trusted store socket and the cloud HTTP client. The split keeps
the upload path in one place and keeps this front door cheap and stateless.

The seam is a pair of small JSON files on the runtime tmpfs:

* the front door writes the request file
  (``/run/ados/logd-push-request.json``) with the window selector
  (``session``, ``since_us``, ``kinds``) plus a correlation id;
* the cloud service watches for the request file, performs the export-and-mark,
  deletes the request, and writes the result file
  (``/run/ados/logd-push-result.json``) carrying the same correlation id and the
  outcome (``pushed``, ``deduped``, ``bytes``, ``rows``, ``synced``, ``error``);
* the front door reads the result back, matching on the correlation id, and
  reports it.

The request is explicit and operator-initiated: nothing in a query, filter, or
refresh path writes it. The cloud service is responsible for refusing the push
when the agent is in local mode, is not cloud-paired, or has cloud log push
disabled — the local store keeps logging to disk regardless, which is the
correct state for an agent that has nothing to sync.

The window selector vocabulary matches the store's export filter so the cloud
service can forward it verbatim:

* ``session`` — restrict to one session id, or ``None`` for any session;
* ``since_us`` — epoch-microsecond lower bound, or ``None`` for the full store;
* ``kinds`` — any subset of ``logs``, ``metrics``, ``events``, ``hw``; an empty
  or absent list means all four.
"""

from __future__ import annotations

import json
import os
import time
import uuid
from dataclasses import dataclass
from datetime import datetime, timezone
from typing import Any

from ados.core.paths import (
    ADOS_RUN_DIR,
    LOGD_PUSH_REQUEST_PATH,
    LOGD_PUSH_RESULT_PATH,
)

# The window kinds the store exports, one table each. An empty selection means
# all four; anything outside this set is rejected before the request is written.
VALID_KINDS: tuple[str, ...] = ("logs", "metrics", "events", "hw")

# How long the front door waits for the cloud service to answer before it
# returns an "accepted, not yet confirmed" result. The export-and-upload runs on
# the cloud service's own loop; a short bounded poll keeps a CLI invocation or an
# HTTP request from hanging on a slow uplink while still catching the common
# fast case.
_DEFAULT_POLL_SECONDS = 8.0
_POLL_INTERVAL_SECONDS = 0.2


class LogPushTriggerError(Exception):
    """A malformed push request that fails before the trigger is written.

    Carries a stable ``code`` so the CLI and the HTTP route can map it onto a
    clean message or status without re-parsing the human text.
    """

    def __init__(self, code: str, message: str) -> None:
        super().__init__(message)
        self.code = code
        self.message = message


@dataclass(frozen=True)
class PushRequest:
    """A validated window selector ready to be written as a trigger file."""

    session: int | None
    since_us: int | None
    kinds: list[str]


def parse_since(value: str | None) -> int | None:
    """Resolve a ``--since`` string to an epoch-microsecond lower bound.

    Accepts the same three forms the store's query API accepts, so the bound the
    operator types lines up with what the export reads:

    * a relative duration with a leading ``-`` and a unit suffix
      (``-90s``, ``-5m``, ``-2h``, ``-1d``, ``-500ms``), resolved against now;
    * an absolute epoch-microsecond integer;
    * a bare ``YYYY-MM-DDTHH:MM:SS`` (or space-separated) timestamp read as UTC.

    Returns ``None`` for an empty/absent value (no lower bound). Raises
    :class:`LogPushTriggerError` with code ``bad_since`` on a malformed input.
    """
    if value is None:
        return None
    s = value.strip()
    if not s:
        return None

    now_us = int(time.time() * 1_000_000)

    if s.startswith("-"):
        micros = _parse_relative(s[1:])
        if micros is None:
            raise LogPushTriggerError(
                "bad_since",
                f"{s!r} is not a relative duration like -5m, -2h, -1d",
            )
        return now_us - micros

    try:
        return int(s)
    except ValueError:
        pass

    iso = _parse_iso_utc(s)
    if iso is not None:
        return iso

    raise LogPushTriggerError(
        "bad_since",
        f"{s!r} is not an epoch-microsecond integer, an ISO timestamp, "
        "or a relative duration like -5m",
    )


def _parse_relative(rest: str) -> int | None:
    """Parse a ``90s`` / ``5m`` / ``2h`` / ``1d`` / ``500ms`` magnitude+unit into
    microseconds. Returns ``None`` on a malformed input."""
    rest = rest.strip()
    split = next((i for i, c in enumerate(rest) if c.isascii() and c.isalpha()), None)
    if split is None:
        return None
    num, unit = rest[:split], rest[split:]
    try:
        magnitude = int(num.strip())
    except ValueError:
        return None
    if magnitude < 0:
        return None
    per_unit_us = {
        "ms": 1_000,
        "s": 1_000_000,
        "m": 60 * 1_000_000,
        "h": 3_600 * 1_000_000,
        "d": 86_400 * 1_000_000,
    }.get(unit)
    if per_unit_us is None:
        return None
    return magnitude * per_unit_us


def _parse_iso_utc(s: str) -> int | None:
    """Parse a bare ``YYYY-MM-DDTHH:MM:SS`` (space or ``T`` separator), read as
    UTC, into an epoch-microsecond integer. Returns ``None`` when it is not that
    shape."""
    try:
        normalized = s.replace(" ", "T").rstrip("Z")
        dt = datetime.fromisoformat(normalized)
    except ValueError:
        return None
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    return int(dt.timestamp() * 1_000_000)


def validate_kinds(kinds: list[str] | None) -> list[str]:
    """Normalize and validate the requested window kinds.

    An empty or absent list selects all four kinds. Any value outside
    :data:`VALID_KINDS` raises :class:`LogPushTriggerError` (``bad_kind``).
    Order is preserved and duplicates are dropped.
    """
    if not kinds:
        return list(VALID_KINDS)
    seen: list[str] = []
    for raw in kinds:
        k = raw.strip().lower()
        if k not in VALID_KINDS:
            raise LogPushTriggerError(
                "bad_kind",
                f"unknown kind {raw!r}; choose from {', '.join(VALID_KINDS)}",
            )
        if k not in seen:
            seen.append(k)
    return seen


def build_request(
    *,
    session: int | None,
    since: str | None,
    kinds: list[str] | None,
) -> PushRequest:
    """Validate the operator's selector into a :class:`PushRequest`.

    Raises :class:`LogPushTriggerError` on a malformed ``since`` or ``kind``.
    """
    return PushRequest(
        session=session,
        since_us=parse_since(since),
        kinds=validate_kinds(kinds),
    )


def write_request(request: PushRequest) -> str:
    """Atomically write the trigger file and return its correlation id.

    The on-disk shape is the shared contract the cloud service consumes::

        {"session": <id|null>, "since_us": <us|null>, "kinds": [...],
         "request_id": "<uuid>", "requested_at_us": <us>}

    The file is written to a temp sibling and renamed so the watcher never reads
    a half-written request. A stale result from a prior push is cleared first so
    the read-back below cannot match the wrong run.
    """
    request_id = uuid.uuid4().hex
    payload: dict[str, Any] = {
        "session": request.session,
        "since_us": request.since_us,
        "kinds": list(request.kinds),
        "request_id": request_id,
        "requested_at_us": int(time.time() * 1_000_000),
    }

    try:
        os.makedirs(ADOS_RUN_DIR, exist_ok=True)
        # Clear any result left by a previous push so the poll below cannot
        # latch onto it. A missing file is fine.
        try:
            os.unlink(LOGD_PUSH_RESULT_PATH)
        except FileNotFoundError:
            pass
        tmp_path = f"{LOGD_PUSH_REQUEST_PATH}.{request_id}.tmp"
        with open(tmp_path, "w", encoding="utf-8") as fh:
            json.dump(payload, fh)
            fh.flush()
            os.fsync(fh.fileno())
        os.replace(tmp_path, LOGD_PUSH_REQUEST_PATH)
    except OSError as exc:
        raise LogPushTriggerError(
            "trigger_unavailable",
            f"could not write the push request: {exc}",
        ) from exc

    return request_id


def read_result(request_id: str, timeout: float = _DEFAULT_POLL_SECONDS) -> dict[str, Any]:
    """Poll briefly for the cloud service's result, matched on the request id.

    Returns the cloud service's result dict on a match, normalized to the shared
    outcome shape. If no matching result lands within ``timeout`` seconds,
    returns an ``accepted`` placeholder (the push may still complete on the cloud
    service's own loop; the trigger has been written either way).
    """
    deadline = time.monotonic() + max(0.0, timeout)
    while True:
        result = _read_result_file()
        if result is not None and result.get("request_id") == request_id:
            return _normalize_result(result, accepted=True)
        if time.monotonic() >= deadline:
            # The trigger is on disk; the cloud service has not answered yet.
            return {
                "accepted": True,
                "request_id": request_id,
                "pushed": False,
                "deduped": False,
                "bytes": 0,
                "rows": 0,
                "synced": False,
                "error": None,
                "pending": True,
            }
        time.sleep(_POLL_INTERVAL_SECONDS)


def _read_result_file() -> dict[str, Any] | None:
    """Read the result file if present and well-formed, else ``None``."""
    try:
        with open(LOGD_PUSH_RESULT_PATH, encoding="utf-8") as fh:
            data = json.load(fh)
    except (FileNotFoundError, ValueError, OSError):
        return None
    return data if isinstance(data, dict) else None


def _normalize_result(raw: dict[str, Any], *, accepted: bool) -> dict[str, Any]:
    """Coerce the cloud service's result onto the stable outcome shape.

    The cloud service writes the authoritative numbers; this only fills in
    defaults for any field an older service omits so the front door always
    returns the same keys.
    """
    error = raw.get("error")
    return {
        "accepted": accepted,
        "request_id": raw.get("request_id"),
        "pushed": bool(raw.get("pushed", False)),
        "deduped": bool(raw.get("deduped", False)),
        "bytes": int(raw.get("bytes", 0) or 0),
        "rows": int(raw.get("rows", 0) or 0),
        "synced": bool(raw.get("synced", False)),
        "window_id": raw.get("window_id"),
        "sha256": raw.get("sha256"),
        "error": str(error) if error else None,
        "pending": False,
    }


def trigger_push(request: PushRequest, *, wait: bool, timeout: float = _DEFAULT_POLL_SECONDS) -> dict[str, Any]:
    """Write the trigger and, when ``wait`` is set, poll briefly for the result.

    With ``wait=False`` the call returns as soon as the trigger is on disk (the
    HTTP 202 path). With ``wait=True`` it waits up to ``timeout`` for the cloud
    service to report back and returns the outcome (or the ``pending``
    placeholder).
    """
    request_id = write_request(request)
    if not wait:
        return {
            "accepted": True,
            "request_id": request_id,
            "pushed": False,
            "deduped": False,
            "bytes": 0,
            "rows": 0,
            "synced": False,
            "error": None,
            "pending": True,
        }
    return read_result(request_id, timeout=timeout)


__all__ = [
    "VALID_KINDS",
    "LogPushTriggerError",
    "PushRequest",
    "build_request",
    "parse_since",
    "read_result",
    "trigger_push",
    "validate_kinds",
    "write_request",
]
