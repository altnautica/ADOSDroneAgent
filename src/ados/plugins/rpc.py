"""RPC envelope and capability tokens for the plugin IPC bridge.

Wire format: length-prefixed msgpack frames over a Unix domain socket.
Each frame is a 4-byte big-endian unsigned length followed by an
:class:`Envelope` dict serialized via msgpack.

Token model: at plugin start the supervisor mints a per-process
HMAC-signed token bound to (plugin_id, granted_caps, session_id, exp).
The token rides in every envelope's ``capability`` field. The
supervisor's IPC server verifies the token before routing the request.

Tokens are short-lived (default 10 min) and rotate on every plugin
restart and on every permission change. The runner does not see the
HMAC secret; it just echoes the token back to the supervisor.
"""

from __future__ import annotations

import hashlib
import hmac
import os
import secrets
import time
from dataclasses import dataclass
from typing import Any, Literal

import msgpack

PROTOCOL_VERSION = 1
TOKEN_TTL_SECONDS = 600
"""Default capability-token lifetime."""


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
    def from_dict(cls, raw: dict[str, Any]) -> "Envelope":
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

    Returns ``None`` on clean EOF (peer closed). Raises
    :class:`FrameError` on protocol errors.
    """
    header = await reader.readexactly(4) if False else await _read_exact(reader, 4)
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
    buf = b""
    while len(buf) < n:
        chunk = await reader.read(n - len(buf))
        if not chunk:
            return None if not buf else None
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
    def from_string(cls, encoded: str) -> "CapabilityToken":
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

    def is_expired(self, now: int | None = None) -> bool:
        ts = now if now is not None else int(time.time())
        return ts >= self.expires_at


class TokenError(Exception):
    """Raised on token shape errors or HMAC mismatch."""


class TokenIssuer:
    """Mints and verifies capability tokens. The supervisor holds a
    single instance; the secret is generated once per supervisor
    process and never written to disk.
    """

    def __init__(self, secret: bytes | None = None) -> None:
        self._secret = secret if secret is not None else secrets.token_bytes(32)

    def mint(
        self,
        *,
        plugin_id: str,
        granted_caps: set[str] | frozenset[str],
        ttl_seconds: int = TOKEN_TTL_SECONDS,
    ) -> CapabilityToken:
        now = int(time.time())
        session_id = secrets.token_hex(8)
        caps = frozenset(granted_caps)
        sig = self._sign(plugin_id, session_id, now, now + ttl_seconds, caps)
        return CapabilityToken(
            plugin_id=plugin_id,
            session_id=session_id,
            granted_caps=caps,
            issued_at=now,
            expires_at=now + ttl_seconds,
            signature=sig,
        )

    def verify(self, token: CapabilityToken) -> None:
        expected = self._sign(
            token.plugin_id,
            token.session_id,
            token.issued_at,
            token.expires_at,
            token.granted_caps,
        )
        if not hmac.compare_digest(expected, token.signature):
            raise TokenError("capability token HMAC mismatch")
        if token.is_expired():
            raise TokenError("capability token expired")

    def _sign(
        self,
        plugin_id: str,
        session_id: str,
        issued_at: int,
        expires_at: int,
        caps: frozenset[str],
    ) -> str:
        payload = "|".join(
            [
                plugin_id,
                session_id,
                str(issued_at),
                str(expires_at),
                ",".join(sorted(caps)),
            ]
        ).encode("utf-8")
        return hmac.new(self._secret, payload, hashlib.sha256).hexdigest()
