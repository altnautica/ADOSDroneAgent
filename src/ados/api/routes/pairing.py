"""Pairing API routes."""

from __future__ import annotations

from fastapi import APIRouter, HTTPException
from pydantic import BaseModel

from ados import __version__
from ados.api.deps import get_agent_app
from ados.core.logging import get_logger
from ados.core.pairing import claim_with_external_code
from ados.core.profile import current_profile_and_role

router = APIRouter(tags=["pairing"])
log = get_logger("pairing_api")


class ClaimRequest(BaseModel):
    user_id: str


class ClaimResponse(BaseModel):
    api_key: str
    device_id: str
    name: str
    mdns_host: str


class PairingInfo(BaseModel):
    device_id: str
    name: str
    version: str
    board: str
    # `paired` is cloud-pair status: the agent has been claimed by a
    # Mission Control account via the Convex API-key flow. Distinct
    # from `radio_paired` below — the two are completely independent
    # and the operator can be in any of the four combinations.
    paired: bool
    # `radio_paired` is radio-pair status: the wfb-ng key handshake
    # with the peer drone or GS has completed, the key files are on
    # disk, and the WFB radio link is authenticated. Surfaces here
    # alongside `paired` so Mission Control can render both states
    # with unambiguous labels.
    radio_paired: bool = False
    # Truncated device-id of the radio-paired peer (16 ASCII chars).
    # Populated from the persisted pair state once the bind tunnel
    # writes it OR a WFB-radio PresenceBeacon back-fills it.
    radio_peer_device_id: str | None = None
    pairing_code: str | None = None
    owner_id: str | None = None
    paired_at: float | None = None
    mdns_host: str
    profile: str
    role: str | None = None
    # Native-vs-packaged aggregate for the node ("native" | "hybrid" |
    # "packaged"), scoped to the profile above. Lets the LAN pairing
    # probe carry the same per-node runtime badge the cloud heartbeat
    # does. Defaults to "packaged" on a pre-cutover agent.
    runtime_mode: str = "packaged"
    # Folded WFB bind-session snapshot read from the cross-process bind
    # sentinel. Null when no bind has run since boot. Lets Mission Control
    # render "binding…" / "bind failed: <error>" during/after a radio pair.
    bind_state: dict | None = None
    # Folded radio link snapshot {state, rssi_dbm, packets_received}. Null
    # from this process today (the radio manager lives in ados-wfb); reserved
    # for a future in-process reader. The GCS falls back to radio_paired.
    radio: dict | None = None
    # Whether a flight controller is connected and talking to this agent. Lets
    # Mission Control tie the agent node to its FC and show FC presence on the
    # single canonical fleet card. Best-effort; False on a partially-configured
    # agent. fc_port/fc_baud are the live serial link parameters.
    fc_connected: bool = False
    fc_port: str | None = None
    fc_baud: int | None = None


@router.get("/pairing/info", response_model=PairingInfo)
async def get_pairing_info():
    """Get pairing info. No auth required.

    Doubles as the Mission Control "probe" endpoint when a user pastes
    a hostname into Add-a-Node — the response carries the node identity
    (device_id, name, board, version), pairing state, and the
    ``profile`` + ``role`` discriminators that drive GCS panel selection.

    Every field read is guarded: a partially-configured agent (fresh
    flash, profile not yet picked, board detect not yet run) used to
    surface as a 500 here, which broke the GCS pairing-probe flow.
    Defaults stand in for missing identity fields so the response is
    always a 200 with a usable shape.
    """
    try:
        app = get_agent_app()
        pm = app.pairing_manager
        info = pm.get_info() if pm is not None else {"paired": False}

        device_id = str(getattr(app.config.agent, "device_id", "") or "")
        name = str(getattr(app.config.agent, "name", "") or "ADOS Agent")
        board = str(app.board_name or "unknown")
        short_id = device_id[:6].lower() or "unknown"

        try:
            profile, role = current_profile_and_role(app.config)
        except Exception as exc:
            log.warning("pairing_info_profile_lookup_failed", error=str(exc))
            profile, role = "drone", None

        # Native-vs-packaged aggregate, scoped to the resolved profile.
        # Best-effort so a partially-configured agent never breaks the
        # guaranteed-200 contract.
        try:
            from ados.core.runtime_mode import compute_runtime_mode
            runtime_mode = compute_runtime_mode(profile)
        except Exception as exc:
            log.debug("pairing_info_runtime_mode_failed", error=str(exc))
            runtime_mode = "packaged"

        discovery = app.discovery_service
        mdns_host = f"ados-{short_id}.local"
        if discovery is not None:
            try:
                mdns_host = str(discovery.mdns_hostname) or mdns_host
            except Exception as exc:
                log.warning("pairing_info_mdns_lookup_failed", error=str(exc))

        # Radio-pair state is owned by ados-wfb (drone) / ados-wfb-rx
        # (GS), separate processes from ados-api. Read directly from
        # config + the on-disk key file rather than via a manager
        # attribute that lives in a different process.
        radio_paired = False
        radio_peer_device_id: str | None = None
        try:
            wfb_cfg = getattr(app.config.video, "wfb", None)
            if wfb_cfg is not None:
                peer_raw = getattr(wfb_cfg, "paired_with_device_id", None)
                if isinstance(peer_raw, str) and peer_raw:
                    radio_peer_device_id = peer_raw
        except Exception as exc:
            log.debug("pairing_info_config_read_failed", error=str(exc))
        try:
            from ados.services.wfb.key_mgr import key_exists
            # key_exists takes (key_dir, role) — passing role=None
            # accepts either tx.key or rx.key as a paired signal.
            radio_paired = bool(key_exists())
        except Exception as exc:
            log.debug("pairing_info_key_check_failed", error=str(exc))

        # Fold the WFB bind-session snapshot from the cross-process sentinel
        # the supervisor writes on every bind transition. Absent file (no bind
        # has run) leaves bind_state None. Each field is .get()-guarded so a
        # partial sentinel never breaks the guaranteed-200 contract.
        bind_state: dict | None = None
        try:
            import json as _json
            from pathlib import Path as _Path

            from ados.services.wfb.bind_client import BIND_STATE_SENTINEL
            sess = _json.loads(_Path(BIND_STATE_SENTINEL).read_text(encoding="utf-8"))
            if isinstance(sess, dict) and sess.get("state"):
                bind_state = {
                    "state": sess.get("state"),
                    "phase": sess.get("phase"),
                    "active": bool(sess.get("active", False)),
                    "error": sess.get("error"),
                    "finished_at": sess.get("finished_at"),
                    "fingerprint": sess.get("fingerprint"),
                }
        except Exception as exc:
            log.debug("pairing_info_bind_state_failed", error=str(exc))

        # FC presence (best-effort; never breaks the guaranteed-200 contract).
        fc_connected = False
        fc_port: str | None = None
        fc_baud: int | None = None
        try:
            fc = app.fc_status()
            fc_connected = bool(fc.connected)
            fc_port = str(fc.port) if fc.port else None
            fc_baud = int(fc.baud) if fc.baud else None
        except Exception as exc:
            log.debug("pairing_info_fc_status_failed", error=str(exc))

        return PairingInfo(
            device_id=device_id,
            name=name,
            version=__version__,
            board=board,
            paired=bool(info.get("paired", False)),
            radio_paired=radio_paired,
            radio_peer_device_id=radio_peer_device_id,
            pairing_code=info.get("pairing_code"),
            owner_id=info.get("owner_id"),
            paired_at=info.get("paired_at"),
            mdns_host=mdns_host,
            profile=profile,
            role=role,
            runtime_mode=runtime_mode,
            bind_state=bind_state,
            radio=None,
            fc_connected=fc_connected,
            fc_port=fc_port,
            fc_baud=fc_baud,
        )
    except HTTPException:
        raise
    except Exception as exc:
        log.exception("pairing_info_unhandled", error=str(exc))
        raise HTTPException(
            status_code=500,
            detail=f"Internal error building pairing info: {type(exc).__name__}",
        ) from exc


@router.get("/pairing/code")
async def get_pairing_code():
    """Get just the pairing code. No auth required."""
    app = get_agent_app()
    pm = app.pairing_manager
    if pm.is_paired:
        raise HTTPException(status_code=409, detail="Already paired")
    return {"code": pm.get_or_create_code()}


@router.post("/pairing/claim", response_model=ClaimResponse)
async def claim_pairing(request: ClaimRequest):
    """Claim this agent for a user (local pairing). No auth required, only works when unpaired."""
    app = get_agent_app()
    pm = app.pairing_manager
    if pm.is_paired:
        raise HTTPException(status_code=409, detail="Already paired. Unpair first.")

    api_key = pm.claim(request.user_id)
    discovery = app.discovery_service
    short_id = app.config.agent.device_id[:6].lower()
    profile, role = current_profile_and_role(app.config)

    # Update mDNS TXT records
    if discovery:
        await discovery.update_txt(
            paired=True,
            owner=request.user_id,
            profile=profile,
            role=role,
        )

    return ClaimResponse(
        api_key=api_key,
        device_id=app.config.agent.device_id,
        name=app.config.agent.name,
        mdns_host=discovery.mdns_hostname if discovery else f"ados-{short_id}.local",
    )


class AcceptCodeRequest(BaseModel):
    code: str


class AcceptCodeResponse(BaseModel):
    ok: bool
    error: str | None = None
    message: str | None = None
    owner_id: str | None = None
    paired_at: float | None = None
    device_id: str | None = None


@router.post("/pairing/accept", response_model=AcceptCodeResponse)
async def accept_pairing_code(request: AcceptCodeRequest):
    """Accept a pairing code that was generated by Mission Control.

    Lets an operator pre-allocate a code on the Mission Control side and
    type it directly into this device's setup wizard, instead of typing
    the device code into Mission Control.
    """
    app = get_agent_app()
    result = await claim_with_external_code(app, request.code)
    if result.get("ok"):
        return AcceptCodeResponse(
            ok=True,
            owner_id=result.get("owner_id"),
            paired_at=result.get("paired_at"),
            device_id=result.get("device_id"),
        )
    return AcceptCodeResponse(
        ok=False,
        error=result.get("error"),
        message=result.get("message"),
    )


@router.post("/pairing/unpair")
async def unpair():
    """Unpair this agent. Requires valid API key (enforced by auth middleware)."""
    app = get_agent_app()
    pm = app.pairing_manager
    if not pm.is_paired:
        raise HTTPException(status_code=409, detail="Not paired")

    pm.unpair()
    discovery = app.discovery_service
    profile, role = current_profile_and_role(app.config)

    # Update mDNS with new code
    new_code = pm.get_or_create_code()
    if discovery:
        await discovery.update_txt(
            paired=False,
            code=new_code,
            profile=profile,
            role=role,
        )

    return {"status": "unpaired", "new_code": new_code}
