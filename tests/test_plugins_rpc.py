"""Plugin RPC envelope, framing, and capability-token tests."""

from __future__ import annotations

import asyncio
import time

import pytest

from ados.plugins.rpc import (
    MAX_FRAME_BYTES,
    CapabilityToken,
    Envelope,
    FrameError,
    TokenError,
    TokenIssuer,
    encode_frame,
    read_frame,
)


# ---------------------------------------------------------------------
# Envelope round-trip
# ---------------------------------------------------------------------


def test_envelope_to_dict_round_trip() -> None:
    env = Envelope(
        type="request",
        method="event.publish",
        capability="event.publish",
        args={"topic": "plugin.com.example.x.alert", "payload": {"k": 1}},
        request_id="r1",
        token="v1.com.example.x.s.0.0..deadbeef",
        error=None,
    )
    again = Envelope.from_dict(env.to_dict())
    assert again == env


def test_envelope_default_version_is_1() -> None:
    env = Envelope.from_dict(
        {"t": "request", "m": "ping", "c": "", "a": {}, "id": "r1", "tok": ""}
    )
    assert env.version == 1


# ---------------------------------------------------------------------
# Wire framing
# ---------------------------------------------------------------------


class _AsyncByteReader:
    """Minimal asyncio.StreamReader stand-in used for read_frame."""

    def __init__(self, data: bytes) -> None:
        self._data = data
        self._pos = 0

    async def read(self, n: int) -> bytes:
        chunk = self._data[self._pos : self._pos + n]
        self._pos += len(chunk)
        return chunk


@pytest.mark.asyncio
async def test_frame_round_trip_through_reader() -> None:
    env = Envelope(
        type="response",
        method="response",
        capability="",
        args={"delivered": 3},
        request_id="r1",
        token="",
    )
    reader = _AsyncByteReader(encode_frame(env))
    got = await read_frame(reader)
    assert got is not None
    assert got.args == {"delivered": 3}


@pytest.mark.asyncio
async def test_zero_length_frame_rejected() -> None:
    reader = _AsyncByteReader(b"\x00\x00\x00\x00")
    with pytest.raises(FrameError):
        await read_frame(reader)


@pytest.mark.asyncio
async def test_oversized_frame_rejected_on_decode() -> None:
    # length field claims more than the cap. read_frame must abort
    # before allocating.
    bad_len = (MAX_FRAME_BYTES + 1).to_bytes(4, "big")
    reader = _AsyncByteReader(bad_len)
    with pytest.raises(FrameError):
        await read_frame(reader)


def test_oversized_frame_rejected_on_encode() -> None:
    big_args = {"blob": "x" * (MAX_FRAME_BYTES + 1)}
    env = Envelope(
        type="request",
        method="event.publish",
        capability="event.publish",
        args=big_args,
        request_id="r1",
        token="",
    )
    with pytest.raises(FrameError):
        encode_frame(env)


@pytest.mark.asyncio
async def test_clean_eof_returns_none() -> None:
    reader = _AsyncByteReader(b"")
    assert await read_frame(reader) is None


# ---------------------------------------------------------------------
# Capability tokens
# ---------------------------------------------------------------------


def test_token_mint_and_verify_round_trip() -> None:
    issuer = TokenIssuer()
    token = issuer.mint(
        plugin_id="com.example.x",
        granted_caps={"event.publish", "event.subscribe"},
    )
    assert token.plugin_id == "com.example.x"
    assert token.granted_caps == frozenset({"event.publish", "event.subscribe"})
    assert not token.is_expired()
    issuer.verify(token)  # does not raise


def test_token_string_round_trip() -> None:
    issuer = TokenIssuer()
    token = issuer.mint(
        plugin_id="com.example.x",
        granted_caps={"event.publish"},
    )
    encoded = token.to_string()
    decoded = CapabilityToken.from_string(encoded)
    assert decoded == token
    issuer.verify(decoded)


def test_token_tampered_caps_rejected() -> None:
    issuer = TokenIssuer()
    token = issuer.mint(plugin_id="com.example.x", granted_caps={"event.publish"})
    tampered = CapabilityToken(
        plugin_id=token.plugin_id,
        session_id=token.session_id,
        granted_caps=frozenset({"event.publish", "event.subscribe"}),  # added scope
        issued_at=token.issued_at,
        expires_at=token.expires_at,
        signature=token.signature,
    )
    with pytest.raises(TokenError):
        issuer.verify(tampered)


def test_token_expired_rejected() -> None:
    issuer = TokenIssuer()
    token = issuer.mint(plugin_id="com.example.x", granted_caps={"event.publish"})
    expired = CapabilityToken(
        plugin_id=token.plugin_id,
        session_id=token.session_id,
        granted_caps=token.granted_caps,
        issued_at=token.issued_at - 10000,
        expires_at=int(time.time()) - 1,
        signature=token.signature,
    )
    with pytest.raises(TokenError):
        issuer.verify(expired)


def test_token_from_malformed_string_raises() -> None:
    with pytest.raises(TokenError):
        CapabilityToken.from_string("not|a|valid|token")  # too few fields
    with pytest.raises(TokenError):
        CapabilityToken.from_string(
            "v1|id|s|notanumber|999|deadbeef|deadbeef"
        )


def test_token_round_trip_with_dotted_plugin_id() -> None:
    """Plugin ids contain dots; the token format must survive that."""
    issuer = TokenIssuer()
    token = issuer.mint(
        plugin_id="com.example.deeply.nested.id",
        granted_caps={"event.publish", "event.subscribe"},
    )
    decoded = CapabilityToken.from_string(token.to_string())
    assert decoded == token
    issuer.verify(decoded)


def test_two_issuers_with_different_secrets_reject_each_others_tokens() -> None:
    a = TokenIssuer()
    b = TokenIssuer()
    token_from_a = a.mint(plugin_id="com.example.x", granted_caps={"event.publish"})
    with pytest.raises(TokenError):
        b.verify(token_from_a)
