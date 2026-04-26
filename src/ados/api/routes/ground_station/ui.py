"""Ground-station UI surfaces.

Covers:
* /ui (OLED, buttons, screens persisted UI config)
* /captive-token (single-use token for the setup webapp)
* /factory-reset (pair + mesh wipe with fingerprint confirm)
* /display (HDMI kiosk config)
* /bluetooth (gamepad scan, pair, forget, list)
* /gamepads (list + primary selection)
* /pic (claim, release, confirm-token, heartbeat, websocket events)
"""

from __future__ import annotations

import asyncio
from pathlib import Path
from typing import Any

import structlog

from fastapi import (
    APIRouter,
    HTTPException,
    Query,
    Request,
    WebSocket,
    WebSocketDisconnect,
)

from ados.api.deps import get_agent_app
from ados.api.routes import ground_station as _gs
from ados.api.routes.ground_station._common import (
    BluetoothPairRequest,
    BluetoothScanRequest,
    ButtonsUpdate,
    DisplayUpdate,
    GamepadPrimaryUpdate,
    OledUpdate,
    PicClaimRequest,
    PicConfirmTokenRequest,
    PicHeartbeatRequest,
    PicReleaseRequest,
    ScreensUpdate,
)
from ados.core.paths import MESH_GATEWAY_JSON


log = structlog.get_logger("ground_station.ui")

router = APIRouter(prefix="/v1/ground-station", tags=["ground-station"])


# ---------------------------------------------------------------------------
# /ui
# ---------------------------------------------------------------------------


@router.get("/ui")
async def get_ground_station_ui() -> dict[str, Any]:
    """Return the full UI config (OLED, buttons, screens)."""
    _gs._require_ground_profile()
    return _gs._load_ui_config()


@router.put("/ui/oled")
async def put_ground_station_ui_oled(update: OledUpdate) -> dict[str, Any]:
    """Update OLED settings, persist to config.yaml, signal oled_service."""
    app = _gs._require_ground_profile()

    data = _gs._load_ui_config()
    oled = dict(data["oled"])
    if update.brightness is not None:
        oled["brightness"] = update.brightness
    if update.auto_dim_enabled is not None:
        oled["auto_dim_enabled"] = update.auto_dim_enabled
    if update.screen_cycle_seconds is not None:
        oled["screen_cycle_seconds"] = update.screen_cycle_seconds
    data["oled"] = oled

    try:
        _gs._persist_gs_ui_section("oled", oled)
    except OSError as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_UI_SAVE_FAILED", "message": str(exc)}},
        ) from exc

    _gs._refresh_in_memory_ui(app, "oled", oled)

    from ados.services.ui.reload_signal import signal_oled_reload

    signal_oled_reload()
    return data


@router.put("/ui/buttons")
async def put_ground_station_ui_buttons(update: ButtonsUpdate) -> dict[str, Any]:
    """Replace the button mapping. Persisted to config and SIGHUP'd live."""
    app = _gs._require_ground_profile()

    data = _gs._load_ui_config()
    if update.mapping is not None:
        data["buttons"] = {"mapping": dict(update.mapping)}

    try:
        _gs._persist_gs_ui_section("buttons", data["buttons"])
    except OSError as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_UI_SAVE_FAILED", "message": str(exc)}},
        ) from exc

    _gs._refresh_in_memory_ui(app, "buttons", data["buttons"])

    from ados.services.ui.reload_signal import signal_buttons_reload

    signal_buttons_reload()
    return data


@router.put("/ui/screens")
async def put_ground_station_ui_screens(update: ScreensUpdate) -> dict[str, Any]:
    """Update screen order and/or enabled set. SIGHUPs oled_service live."""
    app = _gs._require_ground_profile()

    data = _gs._load_ui_config()
    screens = dict(data["screens"])
    if update.order is not None:
        screens["order"] = list(update.order)
    if update.enabled is not None:
        screens["enabled"] = list(update.enabled)
    data["screens"] = screens

    try:
        _gs._persist_gs_ui_section("screens", screens)
    except OSError as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_UI_SAVE_FAILED", "message": str(exc)}},
        ) from exc

    _gs._refresh_in_memory_ui(app, "screens", screens)

    from ados.services.ui.reload_signal import signal_oled_reload

    signal_oled_reload()
    return data


# ---------------------------------------------------------------------------
# /captive-token
# ---------------------------------------------------------------------------


@router.get("/captive-token")
async def get_captive_token(request: Request) -> dict[str, Any]:
    """Mint a single-use captive-portal token for the setup webapp.

    Gated on the AP subnet (192.168.4.0/24). Hosts connecting over any
    other interface get 403. The token is attached by the webapp as
    `X-ADOS-Captive-Key` on destructive operations.
    """
    _gs._require_ground_profile()

    client_host = request.client.host if request.client else None
    if not _gs._is_ap_subnet_client(client_host):
        raise HTTPException(
            status_code=403,
            detail={"error": {"code": "E_CAPTIVE_ONLY"}},
        )

    from ados.services.setup_webapp.captive_token import get_captive_token_store

    token = get_captive_token_store().generate()
    return {"token": token}


# ---------------------------------------------------------------------------
# /factory-reset
# ---------------------------------------------------------------------------


@router.post("/factory-reset")
async def post_factory_reset(
    request: Request,
    confirm: str = Query(..., description="Current pair key fingerprint or stock token"),
) -> dict[str, Any]:
    """Wipe pair state and AP passphrase. Requires the current fingerprint.

    When the ground station is paired, the confirm token must match the
    active pair key fingerprint. When unpaired, the token must match
    `factory-reset-unpaired`. This stops a casual curl from bricking a
    live device.
    """
    _gs._require_ground_profile()

    # Captive-portal single-use token check. Only factory reset is
    # gated. The header is optional when called from loopback to keep
    # CLI test paths open.
    captive_header = request.headers.get("x-ados-captive-key")
    client_host = request.client.host if request.client else None
    if client_host not in ("127.0.0.1", "::1"):
        from ados.services.setup_webapp.captive_token import get_captive_token_store

        if not captive_header or not get_captive_token_store().consume(captive_header):
            raise HTTPException(
                status_code=401,
                detail={"error": {"code": "E_CAPTIVE_TOKEN_INVALID"}},
            )

    pm = _gs._pair_manager()

    try:
        current = await pm.status()
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_PAIR_STATUS_FAILED", "message": str(exc)}},
        ) from exc

    expected = current.get("key_fingerprint") or _gs._stock_confirm_token()
    if confirm != expected:
        raise HTTPException(
            status_code=400,
            detail={"error": {"code": "E_CONFIRM_MISMATCH"}},
        )

    # Take the node out of any mesh role BEFORE wiping pair state, so
    # mesh services (batman, wfb_relay, wfb_receiver) are stopped and
    # their identity files are not racing with the pair_manager reset.
    # `apply_role("direct")` is a no-op when already direct.
    #
    # Role transition MUST succeed before pair state is wiped. A
    # half-finished factory reset that wipes `/etc/ados/mesh/` while
    # batman-adv is still reading from it leaves the filesystem in a
    # state that usually needs a reboot to recover. On transition
    # failure we 500 and abort the whole reset. The operator can try
    # again once the services are stopped (`ados gs role set direct`
    # manually, then retry factory reset).
    from ados.services.ground_station.pairing_client import (
        clear_persisted_identity,
        has_persisted_identity,
    )
    from ados.services.ground_station.pairing_manager import (
        REVOCATIONS_PATH,
    )
    from ados.services.ground_station.role_manager import (
        ROLE_FILE,
        apply_role,
    )

    mesh_wipe: dict[str, Any] = {"cleared_mesh": False}
    try:
        had_identity = has_persisted_identity()
        await apply_role("direct", reason="factory_reset")
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={
                "error": {
                    "code": "E_FACTORY_RESET_ROLE_FAILED",
                    "message": (
                        f"Could not downgrade to direct role before wipe: {exc}. "
                        "Pair state NOT wiped. Stop mesh services manually and retry."
                    ),
                }
            },
        ) from exc

    try:
        result = await pm.factory_reset()
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_FACTORY_RESET_FAILED", "message": str(exc)}},
        ) from exc

    # Wipe mesh identity files after services are down and pair state
    # is cleared. Safe to call unconditionally; no-ops when the files
    # are already absent.
    if "error" not in mesh_wipe:
        try:
            clear_persisted_identity()
            if REVOCATIONS_PATH.is_file():
                try:
                    REVOCATIONS_PATH.unlink()
                except OSError:
                    pass
            if ROLE_FILE.is_file():
                try:
                    ROLE_FILE.unlink()
                except OSError:
                    pass
            # Gateway pin also lives under /etc/ados/mesh/. Clearing
            # it keeps "factory reset = clean slate" honest so a fresh
            # re-pair does not silently inherit the old operator's
            # preferred gateway MAC.
            gateway_path = MESH_GATEWAY_JSON
            if gateway_path.is_file():
                try:
                    gateway_path.unlink()
                except OSError:
                    pass
            mesh_wipe = {
                "cleared_mesh": had_identity,
                "role": "direct",
            }
        except Exception as exc:
            mesh_wipe = {
                "cleared_mesh": False,
                "error": str(exc),
            }

    if isinstance(result, dict):
        result.setdefault("mesh", mesh_wipe)
    else:
        result = {"result": result, "mesh": mesh_wipe}
    return result


# ---------------------------------------------------------------------------
# /display
# ---------------------------------------------------------------------------


@router.get("/display")
async def get_ground_station_display() -> dict[str, Any]:
    """Return the persisted HDMI kiosk display config."""
    _gs._require_ground_profile()
    return _gs._load_display_config()


@router.put("/display")
async def put_ground_station_display(update: DisplayUpdate) -> dict[str, Any]:
    """Update the HDMI kiosk display config and persist."""
    _gs._require_ground_profile()
    current = _gs._load_display_config()

    allowed_res = {"auto", "720p", "1080p"}
    if update.resolution is not None:
        if update.resolution not in allowed_res:
            raise HTTPException(
                status_code=400,
                detail={"error": {"code": "E_INVALID_RESOLUTION"}},
            )
        current["resolution"] = update.resolution
    if update.kiosk_enabled is not None:
        current["kiosk_enabled"] = bool(update.kiosk_enabled)
    if update.kiosk_target_url is not None:
        current["kiosk_target_url"] = update.kiosk_target_url

    try:
        _gs._save_display_config(current)
    except OSError as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_UI_SAVE_FAILED", "message": str(exc)}},
        ) from exc
    return current


# ---------------------------------------------------------------------------
# /bluetooth
# ---------------------------------------------------------------------------


@router.post("/bluetooth/scan")
async def post_bluetooth_scan(req: BluetoothScanRequest) -> dict[str, Any]:
    """Run a BlueZ scan for nearby gamepads. Default duration 10 s."""
    _gs._require_ground_profile()

    duration = req.duration_s if req.duration_s is not None else 10
    try:
        devices = await _gs._input_manager().scan_bluetooth(duration_s=duration)
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_BT_SCAN_FAILED", "message": str(exc)}},
        ) from exc
    return {"devices": devices or []}


@router.post("/bluetooth/pair")
async def post_bluetooth_pair(req: BluetoothPairRequest) -> dict[str, Any]:
    """Attempt to pair with a Bluetooth device by MAC address."""
    _gs._require_ground_profile()

    try:
        result = await _gs._input_manager().pair_bluetooth(req.mac)
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_BT_PAIR_FAILED", "message": str(exc)}},
        ) from exc

    if isinstance(result, dict):
        return result
    return {"paired": bool(result), "error": None}


@router.delete("/bluetooth/{mac}")
async def delete_bluetooth(mac: str) -> dict[str, Any]:
    """Forget a previously-paired Bluetooth device."""
    _gs._require_ground_profile()

    try:
        result = await _gs._input_manager().forget_bluetooth(mac)
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_BT_FORGET_FAILED", "message": str(exc)}},
        ) from exc

    if isinstance(result, dict):
        return result
    return {"forgotten": bool(result)}


@router.get("/bluetooth/paired")
async def get_bluetooth_paired() -> dict[str, Any]:
    """List paired Bluetooth devices."""
    _gs._require_ground_profile()

    try:
        devices = await _gs._input_manager().paired_bluetooth()
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_BT_LIST_FAILED", "message": str(exc)}},
        ) from exc
    return {"devices": devices or []}


# ---------------------------------------------------------------------------
# /gamepads
# ---------------------------------------------------------------------------


@router.get("/gamepads")
async def get_gamepads() -> dict[str, Any]:
    """List connected gamepads and the current primary device id."""
    _gs._require_ground_profile()

    mgr = _gs._input_manager()
    try:
        devices = mgr.list_gamepads()
        if asyncio.iscoroutine(devices):
            devices = await devices
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_GAMEPAD_LIST_FAILED", "message": str(exc)}},
        ) from exc

    primary_id: str | None = None
    try:
        primary = mgr.get_primary()
        if asyncio.iscoroutine(primary):
            primary = await primary
        if isinstance(primary, dict):
            primary_id = primary.get("device_id") or primary.get("id")
        elif isinstance(primary, str):
            primary_id = primary
    except Exception:
        primary_id = None

    return {"devices": devices or [], "primary_id": primary_id}


@router.put("/gamepads/primary")
async def put_gamepad_primary(update: GamepadPrimaryUpdate) -> dict[str, Any]:
    """Select the primary gamepad used by the PIC arbiter."""
    _gs._require_ground_profile()

    try:
        result = _gs._input_manager().set_primary(update.device_id)
        if asyncio.iscoroutine(result):
            result = await result
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_GAMEPAD_PRIMARY_FAILED", "message": str(exc)}},
        ) from exc

    return {"primary_id": update.device_id, "result": result}


# ---------------------------------------------------------------------------
# /pic
# ---------------------------------------------------------------------------


@router.get("/pic")
async def get_pic_state() -> dict[str, Any]:
    """Return the current PIC state dict."""
    _gs._require_ground_profile()

    try:
        state = _gs._pic_arbiter().get_state()
        if asyncio.iscoroutine(state):
            state = await state
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_PIC_STATE_FAILED", "message": str(exc)}},
        ) from exc
    return state if isinstance(state, dict) else {"state": state}


@router.post("/pic/claim")
async def post_pic_claim(req: PicClaimRequest) -> dict[str, Any]:
    """Claim PIC. Returns 409 with needs_confirm=True when re-claim is required."""
    _gs._require_ground_profile()

    arb = _gs._pic_arbiter()
    try:
        result = arb.claim(
            req.client_id,
            confirm_token=req.confirm_token,
            force=bool(req.force),
        )
        if asyncio.iscoroutine(result):
            result = await result
    except PermissionError as exc:
        # Raised when another client holds PIC and no confirm token was
        # provided. Signal the caller to mint a confirm token and retry.
        raise HTTPException(
            status_code=409,
            detail={
                "error": {"code": "E_PIC_CONFIRM_REQUIRED", "message": str(exc)},
                "needs_confirm": True,
            },
        ) from exc
    except ValueError as exc:
        raise HTTPException(
            status_code=400,
            detail={"error": {"code": "E_PIC_CLAIM_INVALID", "message": str(exc)}},
        ) from exc
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_PIC_CLAIM_FAILED", "message": str(exc)}},
        ) from exc

    if isinstance(result, dict):
        if result.get("needs_confirm") and not result.get("granted"):
            # Soft-reject path: arbiter returns dict rather than raising.
            return {**result, "needs_confirm": True}
        return result
    return {"granted": bool(result), "client_id": req.client_id}


@router.post("/pic/release")
async def post_pic_release(req: PicReleaseRequest) -> dict[str, Any]:
    """Release PIC held by the given client id."""
    _gs._require_ground_profile()

    try:
        result = _gs._pic_arbiter().release(req.client_id)
        if asyncio.iscoroutine(result):
            result = await result
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_PIC_RELEASE_FAILED", "message": str(exc)}},
        ) from exc

    if isinstance(result, dict):
        return result
    return {"released": bool(result), "client_id": req.client_id}


@router.post("/pic/confirm-token")
async def post_pic_confirm_token(req: PicConfirmTokenRequest) -> dict[str, Any]:
    """Mint a short-lived PIC takeover confirmation token."""
    _gs._require_ground_profile()

    try:
        token = _gs._pic_arbiter().create_confirm_token(req.client_id)
        if asyncio.iscoroutine(token):
            token = await token
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_PIC_TOKEN_FAILED", "message": str(exc)}},
        ) from exc

    value: str
    ttl: int = 2
    if isinstance(token, dict):
        value = str(token.get("token", ""))
        ttl = int(token.get("ttl_seconds", 2))
    else:
        value = str(token)

    return {"token": value, "ttl_seconds": ttl}


@router.post("/pic/heartbeat")
async def post_pic_heartbeat(req: PicHeartbeatRequest) -> dict[str, Any]:
    """Refresh the PIC session TTL. 410 if the client does not hold PIC."""
    _gs._require_ground_profile()

    try:
        result = _gs._pic_arbiter().heartbeat(req.client_id)
        if asyncio.iscoroutine(result):
            result = await result
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_PIC_HEARTBEAT_FAILED", "message": str(exc)}},
        ) from exc

    if isinstance(result, dict) and result.get("ok") is False:
        raise HTTPException(
            status_code=int(result.get("status", 410)),
            detail={
                "error": {
                    "code": "E_PIC_NO_ACTIVE_CLAIM",
                    "message": str(result.get("error", "no active claim")),
                    "current_pic": result.get("current_pic"),
                }
            },
        )
    return result if isinstance(result, dict) else {"ok": True}


@router.websocket("/pic/events")
async def ws_pic_events(websocket: WebSocket) -> None:
    """Stream PIC arbiter events as JSON until the client disconnects."""
    # Profile gate before accepting so wrong-profile agents close 1008.
    app = get_agent_app()
    profile = getattr(app.config.agent, "profile", "auto")
    if profile != "ground_station":
        await websocket.close(code=1008, reason="E_PROFILE_MISMATCH")
        return

    await websocket.accept()

    # Lazy import to avoid a circular at module load.
    from ados.services.ground_station.pic_arbiter import get_pic_arbiter as _gpa

    arb = _gpa()
    bus = getattr(arb, "bus", None) or getattr(arb, "event_bus", None)
    if bus is None:
        await websocket.send_json({
            "event": "error",
            "code": "E_PIC_BUS_UNAVAILABLE",
        })
        await websocket.close()
        return

    # Bounded outbound queue. A slow WS client (e.g. on a degraded
    # cellular link) used to grow the queue without limit because the
    # default asyncio.Queue() has no maxsize, which made the QueueFull
    # branch below dead code and gave the agent an OOM vector.
    # Drop-oldest keeps the most recent state events; we also log when
    # we shed events so an operator can see the backpressure.
    _PIC_WS_QUEUE_MAX = 100
    queue: asyncio.Queue[Any] = asyncio.Queue(maxsize=_PIC_WS_QUEUE_MAX)
    dropped = 0

    def _on_event(payload: Any) -> None:
        nonlocal dropped
        try:
            queue.put_nowait(payload)
        except asyncio.QueueFull:
            # Make room for the new event by discarding the oldest, so
            # the client always sees the latest state during sustained
            # backpressure rather than a stale snapshot.
            try:
                queue.get_nowait()
                queue.task_done()
            except asyncio.QueueEmpty:
                pass
            try:
                queue.put_nowait(payload)
            except asyncio.QueueFull:
                pass
            dropped += 1
            # Log every 10 dropped events so an operator can correlate
            # with WS disconnect / reconnect storms in journalctl.
            if dropped % 10 == 1:
                log.warning(
                    "pic_ws_backpressure_drop",
                    dropped_count=dropped,
                    queue_max=_PIC_WS_QUEUE_MAX,
                )

    unsubscribe: Any = None
    try:
        subscribe = getattr(bus, "subscribe", None)
        if callable(subscribe):
            unsubscribe = subscribe(_on_event)
    except Exception:
        unsubscribe = None

    try:
        while True:
            payload = await queue.get()
            try:
                await websocket.send_json(payload if isinstance(payload, dict) else {"event": payload})
            except (WebSocketDisconnect, RuntimeError):
                break
    except WebSocketDisconnect:
        pass
    finally:
        if callable(unsubscribe):
            try:
                unsubscribe()
            except Exception:  # noqa: BLE001
                pass
