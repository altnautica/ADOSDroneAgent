"""RPC envelope, framing and capability-token parsing for the plugin IPC bridge.

Wire format: length-prefixed msgpack frames over a Unix domain socket.
Each frame is a 4-byte big-endian unsigned length followed by an
:class:`Envelope` dict serialized via msgpack.

Token model: the plugin host mints a per-plugin HMAC-signed token bound to
(plugin_id, granted_caps, session_id, exp) and verifies it on every request;
the runner never sees the HMAC secret. It only echoes the token and parses it
(:meth:`CapabilityToken.from_string`) to know its own granted set. The host
rotates tokens ahead of expiry and on every permission change.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Literal

import msgpack

PROTOCOL_VERSION = 1


# ---------------------------------------------------------------------------
# Envelope
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class Envelope:
    type: Literal["request", "response", "event"]
    method: str
    capability: str  # capability id this call is exercising
    args: dict[str, Any]
    request_id: str
    token: str
    version: int = PROTOCOL_VERSION
    error: str | None = None

    def to_dict(self) -> dict[str, Any]:
        return {
            "v": self.version,
            "t": self.type,
            "m": self.method,
            "c": self.capability,
            "a": self.args,
            "id": self.request_id,
            "tok": self.token,
            "err": self.error,
        }

    @classmethod
    def from_dict(cls, raw: dict[str, Any]) -> Envelope:
        return cls(
            version=int(raw.get("v", PROTOCOL_VERSION)),
            type=raw["t"],
            method=raw["m"],
            capability=raw.get("c", ""),
            args=raw.get("a") or {},
            request_id=raw["id"],
            token=raw.get("tok", ""),
            error=raw.get("err"),
        )


# ---------------------------------------------------------------------------
# Wire framing
# ---------------------------------------------------------------------------


MAX_FRAME_BYTES = 4 * 1024 * 1024  # 4 MB hard cap per envelope


class FrameError(Exception):
    """Raised on malformed length-prefix or oversized frames."""


def encode_frame(env: Envelope) -> bytes:
    payload = msgpack.packb(env.to_dict(), use_bin_type=True)
    if len(payload) > MAX_FRAME_BYTES:
        raise FrameError(
            f"envelope payload {len(payload)} bytes exceeds cap {MAX_FRAME_BYTES}"
        )
    return len(payload).to_bytes(4, "big") + payload


async def read_frame(reader) -> Envelope | None:
    """Read one length-prefixed frame from an asyncio StreamReader.

    Returns ``None`` on clean EOF (the peer closed the connection before
    any byte of the next frame). Raises :class:`FrameError` on protocol
    errors, including a length-prefix header that arrives truncated.
    """
    header = await _read_exact(reader, 4)
    if header is None:
        return None
    length = int.from_bytes(header, "big")
    if length == 0 or length > MAX_FRAME_BYTES:
        raise FrameError(f"frame length {length} out of range")
    body = await _read_exact(reader, length)
    if body is None:
        raise FrameError("connection closed mid-frame")
    raw = msgpack.unpackb(body, raw=False)
    if not isinstance(raw, dict):
        raise FrameError(f"frame payload is not a mapping: {type(raw).__name__}")
    return Envelope.from_dict(raw)


async def _read_exact(reader, n: int) -> bytes | None:
    """Read exactly ``n`` bytes.

    Returns the buffer on success. Returns ``None`` only on a clean EOF
    at a frame boundary — the peer closed before sending any of the ``n``
    bytes. A partial read (1..n-1 bytes followed by EOF) is a truncated
    frame, not a clean close, so it raises :class:`FrameError`. Treating a
    truncated header as a clean EOF would silently drop a half-sent frame.
    """
    buf = b""
    while len(buf) < n:
        chunk = await reader.read(n - len(buf))
        if not chunk:
            if not buf:
                return None
            raise FrameError(
                f"connection closed mid-frame: read {len(buf)} of {n} bytes"
            )
        buf += chunk
    return buf


# ---------------------------------------------------------------------------
# Capability tokens
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class CapabilityToken:
    plugin_id: str
    session_id: str
    granted_caps: frozenset[str]
    issued_at: int
    expires_at: int
    signature: str  # hex-encoded HMAC-SHA256

    def to_string(self) -> str:
        """Compact string form, pipe-separated.

        Plugin ids are reverse-DNS so they contain dots. Tokens use ``|``
        as the field separator to avoid ambiguity when parsing.
        Layout: ``v1|<plugin_id>|<session>|<issued>|<exp>|<hex_caps>|<sig>``.
        """
        caps_blob = ",".join(sorted(self.granted_caps)).encode("utf-8").hex()
        return "|".join(
            [
                "v1",
                self.plugin_id,
                self.session_id,
                str(self.issued_at),
                str(self.expires_at),
                caps_blob,
                self.signature,
            ]
        )

    @classmethod
    def from_string(cls, encoded: str) -> CapabilityToken:
        parts = encoded.split("|")
        if len(parts) != 7 or parts[0] != "v1":
            raise TokenError("malformed capability token")
        try:
            caps_blob = bytes.fromhex(parts[5]).decode("utf-8")
        except ValueError as exc:
            raise TokenError(f"capability blob not hex: {exc}") from exc
        caps = (
            frozenset(c for c in caps_blob.split(",") if c) if caps_blob else frozenset()
        )
        try:
            return cls(
                plugin_id=parts[1],
                session_id=parts[2],
                issued_at=int(parts[3]),
                expires_at=int(parts[4]),
                granted_caps=caps,
                signature=parts[6],
            )
        except ValueError as exc:
            raise TokenError(f"timestamp not integer: {exc}") from exc


class TokenError(Exception):
    """Raised on a malformed capability token."""
