"""Volatile-field masking, JSON canonicalization, and header comparison.

Two responses from two independently-written handlers will never be bit-for-bit
identical: each carries its own timestamps, its own process id, and counters
that advance between the two requests. To compare the *meaningful* shape of the
two bodies the harness masks those volatile fields to a fixed sentinel and then
canonicalizes the JSON (sorted keys, compact separators) so a byte comparison
sees only what a handler is supposed to control.

A body that is not JSON (an opaque blob, an empty 204, a plain-text error) is
compared as raw bytes, untouched. Only a JSON object/array body is normalized.

Headers are compared on a stable allowlist: the value-bearing headers a client
relies on (``content-type``, ``cache-control``, and similar), never the
hop-by-hop or per-server headers (``date``, ``server``, ``connection``) that
differ between two front doors by construction.
"""

from __future__ import annotations

import json
from collections.abc import Iterable
from typing import Any

# The sentinel a volatile value is replaced with before comparison. Every masked
# field collapses to this, so two responses that differ only in (say) their
# ``uptime`` compare equal.
VOLATILE_SENTINEL = "<volatile>"

# Field keys whose *values* are volatile between two requests and must be masked
# wherever they appear in the body (at any depth). Matched case-insensitively
# against the JSON object key. Kept deliberately small and explicit; a route
# with extra volatile fields extends the set per-case via ``extra_volatile``.
DEFAULT_VOLATILE_KEYS: frozenset[str] = frozenset(
    {
        # wall-clock / monotonic time in every spelling the agent uses
        "ts",
        "ts_us",
        "ts_ms",
        "timestamp",
        "timestamp_ms",
        "time",
        "now",
        # absolute clock readings in nanoseconds/micros/millis: the wall-clock
        # `time_ns` (and its siblings) advances by the inter-request delta and
        # would otherwise diverge between the native and Python reads.
        "time_ns",
        "time_us",
        "time_ms",
        "now_ns",
        "now_us",
        "now_ms",
        "wall_ns",
        "uptime",
        "uptime_seconds",
        "uptime_s",
        "uptime_ms",
        "started_at",
        "started_at_ms",
        "start_time",
        "last_seen",
        "last_seen_ms",
        "last_poll_ms",
        "updated_at",
        "updated_at_ms",
        "generated_at",
        # process identity, never stable across two handlers
        "pid",
        # free-running counters that advance between the two requests
        "seq",
        "sequence",
        "counter",
        "request_id",
        "elapsed_ms",
        "elapsed_us",
        "monotonic_ns",
    }
)

# Response headers whose values are stable and meaningful enough to compare. Any
# header outside this set (``date``, ``server``, the hop-by-hop set) is dropped
# before comparison so a per-front-door difference is never a false fail.
#
# ``etag`` is deliberately absent: it is a cache validator derived from a content
# hash, not a parity contract. One front emits it and another may not, and the
# value moves with the very volatile fields the body comparison already masks, so
# comparing it would be a false fail.
DEFAULT_HEADER_ALLOWLIST: frozenset[str] = frozenset(
    {
        "content-type",
        "cache-control",
        "content-encoding",
        "content-language",
        "vary",
        "www-authenticate",
        "allow",
        "x-ados-profile",
    }
)

# Headers that are hop-by-hop or per-server by definition and must never be
# compared even if they slip onto an allowlist. Belt-and-suspenders: the
# comparator restricts to the allowlist AND removes these.
_NEVER_COMPARE_HEADERS: frozenset[str] = frozenset(
    {
        "date",
        "server",
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        "content-length",  # may differ purely from the volatile masking above
        "etag",  # a content-hash cache validator, not a parity contract
    }
)


def _mask_value(value: Any, volatile_keys: frozenset[str]) -> Any:
    """Recursively replace every volatile-keyed value with the sentinel.

    A dict has each volatile key's value masked and every remaining value
    recursed into; a list recurses element-wise; a scalar is returned as-is.
    Masking is by key, not by value, so a non-volatile field that happens to
    hold a timestamp-looking number is left untouched (the key is the contract).
    """
    if isinstance(value, dict):
        out: dict[str, Any] = {}
        for key, sub in value.items():
            if isinstance(key, str) and key.lower() in volatile_keys:
                out[key] = VOLATILE_SENTINEL
            else:
                out[key] = _mask_value(sub, volatile_keys)
        return out
    if isinstance(value, list):
        return [_mask_value(item, volatile_keys) for item in value]
    return value


def canonical_json(value: Any) -> bytes:
    """Canonicalize a decoded JSON value to deterministic bytes.

    Keys are sorted at every level and separators are compact, so two objects
    with the same content but different key order or whitespace serialize to the
    identical byte string. Non-ASCII is preserved (``ensure_ascii=False``) so the
    canonical form matches the wire form a handler would emit.
    """
    return json.dumps(
        value, sort_keys=True, separators=(",", ":"), ensure_ascii=False
    ).encode("utf-8")


def normalize_body(
    body: bytes, extra_volatile: Iterable[str] | None = None
) -> bytes:
    """Normalize a response body to a canonical, volatile-masked byte string.

    A JSON object/array body is decoded, has its volatile-keyed values masked,
    and is re-serialized canonically. A body that is not JSON (or is a JSON
    scalar, which carries no maskable keys) is returned unchanged so an opaque or
    plain-text payload is still compared faithfully as raw bytes.
    """
    try:
        decoded = json.loads(body)
    except (ValueError, TypeError):
        return body
    if not isinstance(decoded, (dict, list)):
        # A bare scalar (number / string / bool / null) has no keys to mask; the
        # raw bytes already are the canonical form for comparison purposes.
        return body
    volatile = DEFAULT_VOLATILE_KEYS
    if extra_volatile:
        volatile = volatile | frozenset(k.lower() for k in extra_volatile)
    return canonical_json(_mask_value(decoded, volatile))


def _frame_payload(frame: str) -> str:
    """Reduce one raw SSE frame to its comparable payload.

    A server-sent-events frame is a block of ``field: value`` lines. Comment
    lines (a leading ``:``, used for keep-alive pings) and the volatile ``id:``
    field carry no payload contract, so they are dropped; the remaining lines are
    kept in order. The surviving ``data:`` lines are JSON-normalized when they
    parse as JSON so an embedded timestamp inside the event payload is masked the
    same way a body is.
    """
    kept: list[str] = []
    for line in frame.splitlines():
        if not line or line.startswith(":"):
            continue  # blank or comment/keep-alive line
        if line.startswith("id:"):
            continue  # the SSE event id advances every frame
        if line.startswith("data:"):
            data = line[len("data:") :].lstrip()
            kept.append("data:" + normalize_body(data.encode("utf-8")).decode("utf-8"))
        else:
            kept.append(line)
    return "\n".join(kept)


def normalize_sse_frames(frames: Iterable[str]) -> list[str]:
    """Normalize a sequence of SSE frames to comparable payloads, in order.

    Empty frames (a normalizer dropped every line, e.g. a frame that was only a
    keep-alive comment) are discarded so a stray ping on one transport but not
    the other never desynchronizes the sequence.
    """
    out = [_frame_payload(frame) for frame in frames]
    return [frame for frame in out if frame]


def _normalize_header_map(
    headers: dict[str, str], allowlist: frozenset[str]
) -> dict[str, str]:
    """Lower-case header keys, keep only allowlisted (and never-blocked) headers.

    The ``content-type`` value is reduced to its media type (the part before any
    ``;``) so a difference purely in a ``charset`` parameter ordering is not a
    false fail; every other allowlisted value is compared verbatim.
    """
    out: dict[str, str] = {}
    for key, value in headers.items():
        lkey = key.lower()
        if lkey in _NEVER_COMPARE_HEADERS or lkey not in allowlist:
            continue
        if lkey == "content-type":
            value = value.split(";", 1)[0].strip().lower()
        out[lkey] = value.strip()
    return out


def compare_headers(
    native: dict[str, str],
    python: dict[str, str],
    allowlist: frozenset[str] | None = None,
) -> list[str]:
    """Compare two header maps on the allowlist; return human-readable diffs.

    An empty list means the allowlisted headers match. Each diff names the header
    and the two values (or that one side is missing it), so a report reads as a
    line per offending header.
    """
    allow = allowlist or DEFAULT_HEADER_ALLOWLIST
    nh = _normalize_header_map(native, allow)
    ph = _normalize_header_map(python, allow)
    diffs: list[str] = []
    for key in sorted(set(nh) | set(ph)):
        if key not in nh:
            diffs.append(f"header {key!r}: missing on native, python={ph[key]!r}")
        elif key not in ph:
            diffs.append(f"header {key!r}: native={nh[key]!r}, missing on python")
        elif nh[key] != ph[key]:
            diffs.append(f"header {key!r}: native={nh[key]!r} != python={ph[key]!r}")
    return diffs
