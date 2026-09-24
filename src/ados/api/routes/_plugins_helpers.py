"""Shared helpers for the plugin REST surface.

Two concerns live here so the route module stays under the soft 500-LOC cap:

* ``mint_agent_capability_token`` — HKDF-derived per-pairing HMAC token
  for the LAN-direct install path. The agent issues these on demand at
  ``POST /api/plugins/capability-token`` so the GCS does not need a
  Convex round-trip when it has a direct line to the rig.
* ``write_sidecar`` / ``read_sidecar`` — in-flight install-job state at
  ``/run/ados/plugin_install_<jobId>.json``. Survives a WebSocket
  disconnect so the GCS can re-subscribe and see the current stage.

The HKDF derivation uses ``salt = b"ados/plugin-capability-token/v1"``
with the pairing key as input keying material. Each paired GCS gets a
distinct per-pairing HMAC secret without exchanging anything new — the
pairing key already proves possession of the radio link or the same-
LAN pairing event.
"""

from __future__ import annotations

import base64
import hmac
import json
import os
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from cryptography.hazmat.primitives import hashes
from cryptography.hazmat.primitives.kdf.hkdf import HKDF

from ados.core.logging import get_logger

log = get_logger("api.plugins.helpers")


# ---------------------------------------------------------------------
# WebSocket auth helpers
# ---------------------------------------------------------------------
#
# The install-job progress WebSocket uses the same credentials as every
# other agent stream (see :mod:`ados.api.middleware.ws_auth`):
#
#   * ``X-ADOS-Key`` header — native clients (``ados`` CLI, agent
#     integration tests) that can set arbitrary headers on the
#     handshake.
#   * ``Sec-WebSocket-Protocol: ados-ws-ticket, <ticket>`` — browser
#     clients mint a ticket for :func:`job_stream_ticket_scope` of the
#     job via the native ``POST /api/_ws/ticket`` and hand it to
#     ``new WebSocket(url, ["ados-ws-ticket", ticket])``. The front
#     admits the upgrade on the same self-contained HMAC ticket, so the
#     handshake reaches this route on a paired node. The scope names the
#     job, so a ticket opens that job's stream and no other.

from ados.api.middleware.ws_auth import (
    authenticate_websocket as _authenticate_websocket_unified,
)


def job_stream_ticket_scope(job_id: str) -> str:
    """The ticket scope for one install job's progress stream. Must match
    the per-job scope the native mint allows (ados-control
    ``install_job_scope_is_valid``)."""
    return f"plugins.install_job:{job_id}"


async def authenticate_job_websocket(websocket: Any, job_id: str) -> str | None:
    """Validate either the ``X-ADOS-Key`` header or an ``ados-ws-ticket``
    minted for this job.

    Returns the subprotocol the route should echo back in
    ``websocket.accept(subprotocol=...)`` when the ticket path is
    taken (so the browser handshake completes per RFC 6455), or an
    empty string when the header path is taken (no subprotocol to
    echo), or ``None`` on rejection.
    """
    return await _authenticate_websocket_unified(
        websocket, scope=job_stream_ticket_scope(job_id)
    )


# HKDF salt is fixed by spec so a paired GCS and the agent derive the
# same secret independently. Version suffix lets us rotate without
# coordination if we ever change the derivation.
HKDF_SALT_TOKEN_V1 = b"ados/plugin-capability-token/v1"

# Default lifetime mirrors the spec's 10-min window.
TOKEN_TTL_SECONDS_DEFAULT = 600

# Sidecar lives under /run because the install job is in-memory state
# that should not survive a reboot. Mirrors the wfb-failover sidecar
# layout deliberately so operators recognise the pattern.
SIDECAR_DIR = Path("/run/ados")



# ---------------------------------------------------------------------
# HKDF token mint + verify
# ---------------------------------------------------------------------


@dataclass(frozen=True)
class AgentTokenClaims:
    plugin_id: str
    agent_id: str
    operator_id: str
    expires_at_ms: int
    granted_capabilities: tuple[str, ...]
    issuer: str  # always ``f"agent:{agent_id}"`` for this mint path

    def to_dict(self) -> dict[str, Any]:
        return {
            "pluginId": self.plugin_id,
            "agentId": self.agent_id,
            "operatorId": self.operator_id,
            "expiresAt": self.expires_at_ms,
            "grantedCapabilities": list(self.granted_capabilities),
            "iss": self.issuer,
        }


def derive_agent_token_secret(
    pairing_key: str | bytes, *, salt: bytes = HKDF_SALT_TOKEN_V1
) -> bytes:
    """Derive a 32-byte HMAC secret from the pairing key.

    Salt is fixed by spec. ``info`` is left empty because the agent
    issues exactly one secret per pairing; per-token uniqueness comes
    from the claims block, not the secret.
    """
    ikm = pairing_key.encode("utf-8") if isinstance(pairing_key, str) else pairing_key
    if not ikm:
        raise ValueError("pairing key is empty; agent must be paired to mint tokens")
    hkdf = HKDF(algorithm=hashes.SHA256(), length=32, salt=salt, info=b"")
    return hkdf.derive(ikm)


def _canonical_claims_blob(claims: AgentTokenClaims) -> bytes:
    """Stable JSON serialisation for HMAC input.

    Sorted keys + no whitespace so the agent and any verifier produce
    byte-identical inputs without negotiating a wire format.
    """
    payload = claims.to_dict()
    return json.dumps(payload, sort_keys=True, separators=(",", ":")).encode("utf-8")


def mint_agent_capability_token(
    *,
    plugin_id: str,
    agent_id: str,
    operator_id: str,
    granted_capabilities: list[str] | tuple[str, ...],
    pairing_key: str | bytes,
    ttl_seconds: int = TOKEN_TTL_SECONDS_DEFAULT,
    now_ms: int | None = None,
) -> tuple[str, AgentTokenClaims]:
    """Mint a capability token signed with the per-pairing HMAC secret.

    Returns ``(token_string, claims)`` where ``token_string`` is the
    base64-encoded ``b"<claims_b64>.<sig_b64>"`` form the GCS bridge
    consumes. Claims are returned for callers that want to assert on
    the issued ``expiresAt``.
    """
    issued_at = int(now_ms if now_ms is not None else time.time() * 1000)
    claims = AgentTokenClaims(
        plugin_id=plugin_id,
        agent_id=agent_id,
        operator_id=operator_id,
        expires_at_ms=issued_at + ttl_seconds * 1000,
        granted_capabilities=tuple(sorted(set(granted_capabilities))),
        issuer=f"agent:{agent_id}",
    )
    secret = derive_agent_token_secret(pairing_key)
    blob = _canonical_claims_blob(claims)
    sig = hmac.new(secret, blob, "sha256").digest()
    token = (
        base64.urlsafe_b64encode(blob).decode("ascii").rstrip("=")
        + "."
        + base64.urlsafe_b64encode(sig).decode("ascii").rstrip("=")
    )
    return token, claims


def _b64_decode_padless(s: str) -> bytes:
    pad = (-len(s)) % 4
    return base64.urlsafe_b64decode(s + ("=" * pad))


def parse_token_string(token: str) -> tuple[dict[str, Any], bytes, bytes]:
    """Split a token into (claims_dict, claims_blob, signature_bytes).

    Caller verifies the signature against whichever secret is
    appropriate for the token's ``iss`` field. Used by both the bridge
    and the agent-side ``rpc.py`` verifier.
    """
    if not token or "." not in token:
        raise ValueError("malformed token: missing separator")
    blob_b64, sig_b64 = token.rsplit(".", 1)
    blob = _b64_decode_padless(blob_b64)
    sig = _b64_decode_padless(sig_b64)
    try:
        claims = json.loads(blob.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise ValueError(f"malformed token claims: {exc}") from exc
    if not isinstance(claims, dict):
        raise ValueError("token claims must be a JSON object")
    return claims, blob, sig


def verify_agent_token_signature(
    *, token: str, pairing_key: str | bytes
) -> dict[str, Any]:
    """Verify an ``iss: agent:*`` token with the per-pairing HMAC secret.

    Returns the parsed claims dict on success. Raises ``ValueError`` on
    any signature, shape, or expiry failure.
    """
    claims, blob, sig = parse_token_string(token)
    iss = claims.get("iss", "")
    if not iss.startswith("agent:"):
        raise ValueError(f"unexpected issuer for agent verifier: {iss}")
    secret = derive_agent_token_secret(pairing_key)
    expected = hmac.new(secret, blob, "sha256").digest()
    if not hmac.compare_digest(expected, sig):
        raise ValueError("agent token signature mismatch")
    exp = int(claims.get("expiresAt", 0))
    if exp <= int(time.time() * 1000):
        raise ValueError("agent token expired")
    return claims


# ---------------------------------------------------------------------
# Sidecar JSON for in-flight install jobs
# ---------------------------------------------------------------------


def sidecar_path(job_id: str, *, root: Path | None = None) -> Path:
    base = root if root is not None else SIDECAR_DIR
    safe = "".join(c for c in job_id if c.isalnum() or c in "-_.")
    if not safe:
        raise ValueError("job_id is empty after sanitisation")
    return base / f"plugin_install_{safe}.json"


def write_sidecar(
    job_id: str,
    payload: dict[str, Any],
    *,
    root: Path | None = None,
) -> Path:
    """Update the in-flight job state. Atomic via tmp + rename."""
    path = sidecar_path(job_id, root=root)
    path.parent.mkdir(parents=True, exist_ok=True)
    enriched = dict(payload)
    enriched.setdefault("jobId", job_id)
    enriched["updatedAt"] = int(time.time() * 1000)
    _atomic_write_text(path, json.dumps(enriched, sort_keys=True))
    return path


def read_sidecar(job_id: str, *, root: Path | None = None) -> dict[str, Any] | None:
    path = sidecar_path(job_id, root=root)
    if not path.exists():
        return None
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        log.warning("sidecar_read_failed", path=str(path), error=str(exc))
        return None


def read_sidecar_snapshot(
    job_id: str, *, root: Path | None = None
) -> tuple[dict[str, Any], float] | None:
    """Read the sidecar payload AND its mtime from ONE open file handle.

    The stream loop decides "did the job state change?" from the mtime and
    then sends the payload; those two facts must describe the SAME file
    version. Reading the content with ``read_sidecar`` and then ``stat()``ing
    the mtime separately raced: a write (tmp + atomic rename) landing between
    the two syscalls paired stale content with a fresh mtime, so the loop
    streamed the previous payload under the new change token and then never
    re-sent the real update (the reader saw the new mtime as already-seen).
    Taking both from one open fd makes them a single consistent snapshot —
    an atomic replace happens entirely before or entirely after the open, so
    the fd always yields matching (payload, mtime).
    """
    path = sidecar_path(job_id, root=root)
    try:
        with path.open("r", encoding="utf-8") as handle:
            mtime = os.fstat(handle.fileno()).st_mtime
            payload = json.loads(handle.read())
    except FileNotFoundError:
        return None
    except (OSError, json.JSONDecodeError) as exc:
        log.warning("sidecar_read_failed", path=str(path), error=str(exc))
        return None
    return payload, mtime


def _atomic_write_text(path: Path, body: str) -> None:
    """Write text atomically — tmp file in the same directory + rename."""
    path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(
        mode="w",
        encoding="utf-8",
        dir=str(path.parent),
        delete=False,
    ) as tmp:
        tmp.write(body)
        tmp_path = Path(tmp.name)
    os.replace(tmp_path, path)


TERMINAL_STAGES = frozenset({"completed", "failed", "cancelled"})


# ---------------------------------------------------------------------
# WebSocket stream loop (extracted so the route stays slim)
# ---------------------------------------------------------------------


WS_IDLE_TIMEOUT_SECONDS = 600.0
WS_POLL_INTERVAL_SECONDS = 0.1


async def run_job_progress_stream(
    websocket: Any,
    job_id: str,
    *,
    idle_timeout_seconds: float | None = None,
    poll_interval_seconds: float | None = None,
    sidecar_root: Path | None = None,
) -> None:
    """Poll the in-flight sidecar and stream each change to the socket.

    Caller is responsible for ``websocket.accept()`` before invoking
    so tests can drive the websocket without coupling to FastAPI's
    accept timing. Closes on terminal stage or after ``idle_timeout``.
    """
    import asyncio

    # Resolve module-level knobs at call time so test monkeypatches land.
    idle_timeout = (
        idle_timeout_seconds
        if idle_timeout_seconds is not None
        else WS_IDLE_TIMEOUT_SECONDS
    )
    poll_interval = (
        poll_interval_seconds
        if poll_interval_seconds is not None
        else WS_POLL_INTERVAL_SECONDS
    )

    last_mtime: float = 0.0
    idle_since = time.monotonic()
    while True:
        snapshot = read_sidecar_snapshot(job_id, root=sidecar_root)
        if snapshot is not None:
            payload, mtime = snapshot
            if mtime != last_mtime:
                last_mtime = mtime
                idle_since = time.monotonic()
                await websocket.send_json(payload)
                if payload.get("stage") in TERMINAL_STAGES:
                    return
        if time.monotonic() - idle_since > idle_timeout:
            await websocket.send_json(
                {"stage": "cancelled", "jobId": job_id, "reason": "idle_timeout"}
            )
            return
        await asyncio.sleep(poll_interval)


# ---------------------------------------------------------------------
# Capability-token mint orchestration (extracted from the route)
# ---------------------------------------------------------------------


def compute_granted_caps_for_token(in_memory_permissions: dict[str, Any]) -> list[str]:
    """The capabilities granted right now, from the supervisor's install record.

    Every grant and revoke (LAN or cloud) lands in that record, so the token
    carries exactly the current grant set.
    """
    return sorted(
        pid
        for pid, grant in in_memory_permissions.items()
        if getattr(grant, "granted", False)
    )
