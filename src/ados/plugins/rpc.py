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

The supervisor also accepts a second token shape — JSON claims signed
with one of three issuer secrets — coming through the GCS bridge for
postMessage RPCs. See :class:`MultiIssuerVerifier`.
"""

from __future__ import annotations

import base64
import hashlib
import hmac
import json
import os
import secrets
import time
from dataclasses import dataclass
from typing import Any, Callable, Literal

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


# ---------------------------------------------------------------------------
# Multi-issuer JSON-claim token verifier
# ---------------------------------------------------------------------------


class TokenInvalid(TokenError):
    """Raised when a JSON-claim token fails issuer / signature / claim checks."""


@dataclass(frozen=True)
class VerifiedClaims:
    plugin_id: str
    agent_id: str
    operator_id: str
    expires_at_ms: int
    granted_capabilities: tuple[str, ...]
    issuer: str


def _b64_decode_padless(s: str) -> bytes:
    pad = (-len(s)) % 4
    return base64.urlsafe_b64decode(s + ("=" * pad))


def _split_claim_token(token: str) -> tuple[dict[str, Any], bytes, bytes]:
    if not token or "." not in token:
        raise TokenInvalid("malformed token: missing separator")
    blob_b64, sig_b64 = token.rsplit(".", 1)
    blob = _b64_decode_padless(blob_b64)
    sig = _b64_decode_padless(sig_b64)
    try:
        claims = json.loads(blob.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise TokenInvalid(f"malformed claims: {exc}") from exc
    if not isinstance(claims, dict):
        raise TokenInvalid("claims must be a JSON object")
    return claims, blob, sig


class MultiIssuerVerifier:
    """Verifies JSON-claim tokens for three issuer variants.

    * ``iss = "cloud:<userId>"`` — signature verified with the
      operator's HMAC secret, fetched via the supplied Convex callable
      and cached for five minutes.
    * ``iss = "agent:<deviceId>"`` — signature verified with the
      agent's own HKDF-derived per-pairing secret. Caller supplies the
      derivation as ``agent_secret_provider``.
    * ``iss = "local"`` — dev-mode CLI token; signature is still
      verified against a local dev secret but the ``agentId`` claim is
      not enforced.

    All variants require ``expires_at_ms`` in the future and
    ``agent_id`` matching ``self.device_id`` (except ``iss=local``).
    """

    OPERATOR_SECRET_TTL_SECONDS = 300

    def __init__(
        self,
        *,
        device_id: str,
        agent_secret_provider: Callable[[], bytes] | None = None,
        operator_secret_fetcher: Callable[[str], bytes | None] | None = None,
        local_dev_secret: bytes | None = None,
        now_ms: Callable[[], int] | None = None,
    ) -> None:
        self.device_id = device_id
        self._agent_secret_provider = agent_secret_provider
        self._operator_secret_fetcher = operator_secret_fetcher
        self._local_dev_secret = local_dev_secret
        self._now_ms = now_ms or (lambda: int(time.time() * 1000))
        self._op_cache: dict[str, tuple[bytes, int]] = {}

    def verify(self, token: str) -> VerifiedClaims:
        claims, blob, sig = _split_claim_token(token)
        iss = str(claims.get("iss", ""))
        plugin_id = str(claims.get("pluginId", ""))
        agent_id = str(claims.get("agentId", ""))
        operator_id = str(claims.get("operatorId", ""))
        expires_at = int(claims.get("expiresAt", 0))
        caps = tuple(claims.get("grantedCapabilities") or [])

        if iss.startswith("cloud:"):
            user_id = iss.split(":", 1)[1]
            secret = self._operator_secret(user_id)
            self._require_match(secret, blob, sig, "cloud")
            self._require_device_match(agent_id)
        elif iss.startswith("agent:"):
            if self._agent_secret_provider is None:
                raise TokenInvalid("agent issuer not configured")
            secret = self._agent_secret_provider()
            self._require_match(secret, blob, sig, "agent")
            self._require_device_match(agent_id)
        elif iss == "local":
            if self._local_dev_secret is None:
                raise TokenInvalid("local issuer not enabled")
            self._require_match(self._local_dev_secret, blob, sig, "local")
            # Local dev tokens deliberately skip the agent_id check.
        else:
            raise TokenInvalid(f"unknown issuer: {iss}")

        if expires_at <= self._now_ms():
            raise TokenInvalid("token expired")

        return VerifiedClaims(
            plugin_id=plugin_id,
            agent_id=agent_id,
            operator_id=operator_id,
            expires_at_ms=expires_at,
            granted_capabilities=caps,
            issuer=iss,
        )

    def _operator_secret(self, user_id: str) -> bytes:
        if not user_id:
            raise TokenInvalid("cloud issuer missing userId")
        now = self._now_ms()
        cached = self._op_cache.get(user_id)
        if cached is not None and cached[1] > now:
            return cached[0]
        if self._operator_secret_fetcher is None:
            raise TokenInvalid("cloud issuer not configured")
        secret = self._operator_secret_fetcher(user_id)
        if not secret:
            raise TokenInvalid(f"no operator secret for {user_id}")
        self._op_cache[user_id] = (secret, now + self.OPERATOR_SECRET_TTL_SECONDS * 1000)
        return secret

    @staticmethod
    def _require_match(secret: bytes, blob: bytes, sig: bytes, kind: str) -> None:
        expected = hmac.new(secret, blob, hashlib.sha256).digest()
        if not hmac.compare_digest(expected, sig):
            raise TokenInvalid(f"{kind} signature mismatch")

    def _require_device_match(self, agent_id: str) -> None:
        if agent_id != self.device_id:
            raise TokenInvalid(
                f"agentId claim {agent_id!r} does not match device {self.device_id!r}"
            )
