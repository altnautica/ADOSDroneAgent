"""Shared helpers for the plugin REST and remote-install surfaces.

Three concerns live here so the route module stays under the soft
500-LOC cap and the cloud-relay receiver can reuse the same code paths:

* ``mint_agent_capability_token`` — HKDF-derived per-pairing HMAC token
  for the LAN-direct install path. The agent issues these on demand at
  ``POST /api/plugins/capability-token`` so the GCS does not need a
  Convex round-trip when it has a direct line to the rig.
* ``write_granted_permissions_yaml`` — per-(operator, drone, plugin)
  grant file at ``/var/lib/ados/plugins/<plugin_id>/granted-permissions.yaml``.
  Mirrors the permission-model module's audit-log layout so the cloud
  state and the on-disk state agree.
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
# The plugin install-job WebSocket has its own ticket flow that binds
# each ticket to a specific ``job_id`` and uses the historical
# ``ados-job-ticket`` subprotocol marker. The underlying ticket store
# and the credential-check logic live in
# :mod:`ados.api.middleware.ws_auth` so other routes can reuse the
# same code paths.
#
# Two accepted credentials per the WebSocket auth contract:
#
#   * ``X-ADOS-Key`` header — native clients (``ados`` CLI, agent
#     integration tests) that can set arbitrary headers on the
#     handshake.
#   * ``Sec-WebSocket-Protocol: ados-job-ticket, <ticket-hex>`` — the
#     subprotocol-based ticket flow for browser clients. The GCS
#     first mints a one-shot ticket via
#     ``POST /api/plugins/jobs/{job_id}/ticket`` (which the HTTP
#     middleware still authenticates with the pairing key) and then
#     hands the ticket to ``new WebSocket(url, ["ados-job-ticket",
#     ticket])``. The ticket is consumed on first use and expires
#     within 30 s. Replaces the previous ``?api_key=`` query-string
#     fallback so the pairing key never reaches DevTools, HAR
#     exports, or reverse-proxy access logs.


from ados.api.middleware.ws_auth import (
    WS_JOB_TICKET_PROTOCOL,
    WS_TICKET_PROTOCOL as _WS_TICKET_PROTOCOL,
    authenticate_websocket as _authenticate_websocket_unified,
    ws_ticket_store,
)

# Re-export so the test suite and any external callers can pick the
# marker up from the historical location.
__all_ws = ["WS_JOB_TICKET_PROTOCOL", "_WS_TICKET_PROTOCOL"]


def _job_ticket_scope(job_id: str) -> str:
    """Map an install-job id onto the unified ticket store's scope key."""
    return f"plugin-job:{job_id}"


class _JobTicketStoreShim:
    """Thin shim around the unified ticket store.

    Existing tests reach into ``job_ticket_store`` to issue or reset
    tickets without going through the route layer. Preserve that
    surface while the underlying storage moves to the unified store.
    """

    @property
    def HEX_LEN(self) -> int:  # noqa: N802 — back-compat constant name
        return ws_ticket_store.HEX_LEN

    async def issue(
        self,
        job_id: str,
        *,
        ttl_seconds: int = 30,
        now_ms: int | None = None,
    ) -> tuple[str, int]:
        return await ws_ticket_store.issue(
            _job_ticket_scope(job_id),
            ttl_seconds=ttl_seconds,
            now_ms=now_ms,
        )

    async def consume(
        self,
        ticket: str,
        *,
        job_id: str,
        now_ms: int | None = None,
    ) -> bool:
        return await ws_ticket_store.consume(
            ticket,
            scope=_job_ticket_scope(job_id),
            now_ms=now_ms,
        )

    def _reset_for_tests(self) -> None:
        ws_ticket_store._reset_for_tests()


# Module-level singleton kept for back-compat with tests and any
# external caller that imported the symbol directly.
job_ticket_store = _JobTicketStoreShim()


async def authenticate_job_websocket(
    websocket: Any,
    *,
    job_id: str,
) -> str | None:
    """Validate either the ``X-ADOS-Key`` header or a one-shot ticket.

    Returns the subprotocol the route should echo back in
    ``websocket.accept(subprotocol=...)`` when the ticket path is
    taken (so the browser handshake completes per RFC 6455), or an
    empty string when the header path is taken (no subprotocol to
    echo), or ``None`` on rejection.
    """
    return await _authenticate_websocket_unified(
        websocket,
        scope=_job_ticket_scope(job_id),
        allow_legacy_job_protocol=True,
    )


async def authenticate_websocket(websocket: Any) -> bool:
    """Back-compat header-only wrapper for routes without a ticket flow.

    New routes should call
    :func:`ados.api.middleware.ws_auth.authenticate_websocket` with an
    explicit ``scope``. Returns ``True`` on success, ``False`` on
    failure (in which case the socket has been closed with ``4401``).
    """
    from ados.api.deps import get_agent_app

    app = get_agent_app()
    pm = getattr(app, "pairing_manager", None)

    if pm is None or not getattr(pm, "is_paired", False):
        return True

    configured_key: str | None = None
    try:
        configured_key = app.config.security.api.api_key
    except AttributeError:
        configured_key = None

    api_key = websocket.headers.get("X-ADOS-Key")
    if api_key:
        if configured_key and api_key == configured_key:
            return True
        if pm.validate_key(api_key):
            return True

    await websocket.close(code=4401, reason="auth required")
    return False

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

# Granted-permissions audit log lives under /var/lib so it survives a
# reboot. One file per plugin id; the grant rows inside it are keyed
# by (operator_id, agent_id) so a single agent can support multiple
# operators (a rare case today but cheap to model).
GRANTS_DIR = Path("/var/lib/ados/plugins")


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
# Granted-permissions audit log
# ---------------------------------------------------------------------


def grant_file_path(plugin_id: str, *, root: Path | None = None) -> Path:
    """Filesystem location of the granted-permissions audit row."""
    base = root if root is not None else GRANTS_DIR
    return base / plugin_id / "granted-permissions.yaml"


def write_granted_permissions_yaml(
    *,
    plugin_id: str,
    operator_id: str,
    agent_id: str,
    granted: list[str],
    granted_at_ms: int | None = None,
    root: Path | None = None,
) -> Path:
    """Write the per-(operator, drone, plugin) grant record.

    YAML is hand-rolled to avoid a runtime ``yaml`` import inside the
    install hot path. The output is still valid YAML 1.2 / JSON-superset
    so any consumer (audit log reader, debug dump) parses it cleanly.
    """
    base = root if root is not None else GRANTS_DIR
    target = base / plugin_id
    target.mkdir(parents=True, exist_ok=True)
    grant_path = target / "granted-permissions.yaml"
    ts = int(granted_at_ms if granted_at_ms is not None else time.time() * 1000)
    # Cheap YAML emitter — list of strings as a JSON-compatible flow
    # sequence so a downstream JSON-only reader still parses it.
    granted_str = (
        "[" + ", ".join(json.dumps(p) for p in sorted(set(granted))) + "]"
    )
    body = (
        f"operator_id: {json.dumps(operator_id)}\n"
        f"agent_id: {json.dumps(agent_id)}\n"
        f"plugin_id: {json.dumps(plugin_id)}\n"
        f"granted: {granted_str}\n"
        f"granted_at_ms: {ts}\n"
    )
    _atomic_write_text(grant_path, body)
    try:
        os.chmod(grant_path, 0o640)
    except OSError as exc:
        log.warning("granted_perms_chmod_failed", path=str(grant_path), error=str(exc))
    return grant_path


def read_granted_permissions_yaml(
    plugin_id: str, *, root: Path | None = None
) -> dict[str, Any] | None:
    """Inverse of :func:`write_granted_permissions_yaml`.

    Returns the parsed record or ``None`` if no grant file exists.
    Tolerant of the JSON-compatible flow form used by the writer; this
    is a thin emitter, not a real YAML library.
    """
    grant_path = grant_file_path(plugin_id, root=root)
    if not grant_path.exists():
        return None
    try:
        text = grant_path.read_text(encoding="utf-8")
    except OSError as exc:
        log.warning(
            "granted_perms_read_failed", path=str(grant_path), error=str(exc)
        )
        return None
    out: dict[str, Any] = {}
    for line in text.splitlines():
        line = line.rstrip()
        if not line or line.startswith("#"):
            continue
        if ":" not in line:
            continue
        key, _, raw = line.partition(":")
        raw = raw.strip()
        try:
            out[key.strip()] = json.loads(raw) if raw else None
        except json.JSONDecodeError:
            out[key.strip()] = raw.strip('"')
    return out


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


def clear_sidecar(job_id: str, *, root: Path | None = None) -> None:
    path = sidecar_path(job_id, root=root)
    try:
        path.unlink(missing_ok=True)
    except OSError as exc:
        log.warning("sidecar_clear_failed", path=str(path), error=str(exc))


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


def is_terminal_stage(stage: str | None) -> bool:
    return bool(stage) and stage in TERMINAL_STAGES


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
        payload = read_sidecar(job_id, root=sidecar_root)
        if payload is not None:
            try:
                mtime = sidecar_path(job_id, root=sidecar_root).stat().st_mtime
            except FileNotFoundError:
                mtime = 0.0
            if mtime != last_mtime:
                last_mtime = mtime
                idle_since = time.monotonic()
                await websocket.send_json(payload)
                if is_terminal_stage(payload.get("stage")):
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


def compute_granted_caps_for_token(
    *,
    plugin_id: str,
    in_memory_permissions: dict[str, Any],
    grants_root: Path | None = None,
) -> tuple[list[str], dict[str, Any] | None]:
    """Resolve the granted-capability list for the mint endpoint.

    Audit file wins when present so a refreshed install picks up the
    latest grant immediately. Falls back to the supervisor's in-memory
    grant map for tests and for the brief window between install and
    the first audit-log flush.
    """
    audit = read_granted_permissions_yaml(plugin_id, root=grants_root)
    granted: list[str] = []
    if audit and isinstance(audit.get("granted"), list):
        granted = [str(p) for p in audit["granted"]]
    if not granted:
        granted = sorted(
            pid
            for pid, grant in in_memory_permissions.items()
            if getattr(grant, "granted", False)
        )
    return granted, audit
