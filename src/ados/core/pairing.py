"""Pairing manager for ADOS Drone Agent."""

from __future__ import annotations

import json
import secrets
import time
from pathlib import Path

from ados.core.atomic import atomic_write_json
from ados.core.logging import get_logger
from ados.core.paths import PAIRING_JSON

log = get_logger("pairing")

# Safe charset: no ambiguous chars (0/O/1/I/L)
SAFE_CHARSET = "ABCDEFGHJKMNPQRSTUVWXYZ23456789"
CODE_LENGTH = 6
CODE_TTL = 900  # 15 minutes
PAIRING_STATE_PATH = str(PAIRING_JSON)


class PairingManager:
    """Manages pairing state, code generation, and API key validation.

    The agent runs three processes that each instantiate this class
    (ados-api, ados-cloud, ados-supervisor). Without cross-process
    coordination they would each carry their own in-memory snapshot
    of ``pairing.json`` and diverge from disk as soon as one of them
    rotated the pair code. The mtime-tracked reload below keeps all
    three convergent within one public-read cycle.
    """

    def __init__(self, state_path: str = PAIRING_STATE_PATH):
        self._state_path = Path(state_path)
        self._state: dict = {}
        self._last_loaded_mtime: float = 0.0
        self._load_state()

    def _load_state(self) -> None:
        if self._state_path.exists():
            try:
                self._state = json.loads(self._state_path.read_text())
                log.info("pairing_state_loaded", paired=self._state.get("paired", False))
            except (json.JSONDecodeError, OSError) as e:
                log.warning("pairing_state_load_failed", error=str(e))
                self._state = {}
        else:
            self._state = {}
        try:
            self._last_loaded_mtime = self._state_path.stat().st_mtime
        except OSError:
            self._last_loaded_mtime = 0.0

    def _maybe_reload(self) -> None:
        """Reload state when the on-disk pairing.json is newer than the
        copy we have in memory.

        Cheap: one stat() call per public read. The file is ~200 bytes,
        the reload itself only fires on mtime change. Without this the
        reading process serves stale state forever — the symptom that
        bit us when ados-cloud rotated the code and ados-api kept
        advertising the old one through /api/pairing/code, /api/pairing/info,
        and `ados status`.
        """
        try:
            mtime = self._state_path.stat().st_mtime
        except OSError:
            return
        if mtime > self._last_loaded_mtime:
            self._load_state()

    def _save_state(self) -> None:
        atomic_write_json(self._state_path, self._state, indent=2)
        try:
            self._last_loaded_mtime = self._state_path.stat().st_mtime
        except OSError:
            pass
        log.debug("pairing_state_saved")

    @staticmethod
    def generate_code() -> str:
        """Generate a human-friendly pairing code."""
        return "".join(secrets.choice(SAFE_CHARSET) for _ in range(CODE_LENGTH))

    @staticmethod
    def generate_api_key() -> str:
        """Generate a secure API key with ados_ prefix."""
        return "ados_" + secrets.token_urlsafe(32)

    @property
    def is_paired(self) -> bool:
        self._maybe_reload()
        return self._state.get("paired", False)

    @property
    def api_key(self) -> str | None:
        self._maybe_reload()
        return self._state.get("api_key") if self._state.get("paired", False) else None

    @property
    def owner_id(self) -> str | None:
        self._maybe_reload()
        return self._state.get("owner_id") if self._state.get("paired", False) else None

    def get_or_create_code(self) -> str:
        """Get current pairing code, or generate a new one if expired.

        Reloads disk state first so a sibling process that just wrote a
        fresh code (atomic write + mtime bump) is observed and returned
        verbatim. Without the reload this would generate a NEW code and
        clobber the sibling's write — exactly the race that gave the
        bench two competing codes per drone.
        """
        self._maybe_reload()
        code = self._state.get("pairing_code")
        created_at = self._state.get("code_created_at", 0)
        if code and (time.time() - created_at) < CODE_TTL:
            return code
        # Generate new code
        code = self.generate_code()
        self._state["pairing_code"] = code
        self._state["code_created_at"] = time.time()
        self._state.pop("paired", None)
        self._save_state()
        log.info("pairing_code_generated", code=code)
        return code

    def set_code(self, code: str) -> None:
        """Set a pre-generated pairing code (from install --pair flag)."""
        self._state["pairing_code"] = code.upper()
        self._state["code_created_at"] = time.time()
        self._save_state()
        log.info("pairing_code_set", code=code.upper())

    def code_expires_at(self) -> int | None:
        """Epoch seconds when the current pairing code stops being valid.

        Returns ``None`` when no code is currently active (already
        paired, or no `code_created_at` recorded). Surfaced on the
        pairing beacon so the GCS can render a countdown clock and
        stop showing a stale code.
        """
        self._maybe_reload()
        created_at = self._state.get("code_created_at")
        if not created_at:
            return None
        return int(created_at) + CODE_TTL

    def claim(self, user_id: str, api_key: str | None = None) -> str:
        """Claim this agent for a user. Returns API key."""
        if self.is_paired:
            raise ValueError("Already paired. Unpair first.")
        key = api_key or self.generate_api_key()
        self._state["paired"] = True
        self._state["api_key"] = key
        self._state["owner_id"] = user_id
        self._state["paired_at"] = time.time()
        self._state.pop("pairing_code", None)
        self._state.pop("code_created_at", None)
        self._save_state()
        log.info("pairing_claimed", user_id=user_id)
        return key

    def unpair(self) -> None:
        """Clear pairing state, generate new code."""
        old_owner = self._state.get("owner_id")
        self._state = {}
        self._save_state()
        log.info("pairing_unpaired", previous_owner=old_owner)

    def validate_key(self, key: str) -> bool:
        """Check if a given API key matches the stored one."""
        self._maybe_reload()
        if not self._state.get("paired", False):
            return True  # When unpaired, all access is open
        return key == self._state.get("api_key")

    def get_info(self) -> dict:
        """Get pairing info for the /pairing/info endpoint."""
        self._maybe_reload()
        if self._state.get("paired", False):
            return {
                "paired": True,
                "owner_id": self._state.get("owner_id"),
                "paired_at": self._state.get("paired_at"),
            }
        return {
            "paired": False,
            "pairing_code": self.get_or_create_code(),
        }


async def claim_with_external_code(app: object, code: str) -> dict:
    """Try to register this agent against a code generated elsewhere.

    Mission Control can pre-allocate a pairing code (so an operator
    types it directly into this device instead of typing the device
    code into Mission Control). This helper runs the matching POST to
    the cloud handshake endpoint, then flips local pairing state when
    the response confirms a match.

    Returns a dict with ``ok: bool`` plus a structured ``error`` /
    ``owner_id`` / ``paired_at`` payload. Network failures and bad
    codes both surface as ``ok: false`` so the wizard can render a
    single error chip.
    """

    import httpx

    cleaned = "".join(ch for ch in (code or "").upper() if ch.isalnum())
    if len(cleaned) != CODE_LENGTH:
        return {
            "ok": False,
            "error": "invalid_code",
            "message": "Pairing code must be 6 characters.",
        }

    pairing_manager = getattr(app, "pairing_manager", None)
    config = getattr(app, "config", None)
    if pairing_manager is None or config is None:
        return {
            "ok": False,
            "error": "agent_not_ready",
            "message": "Agent is not initialised.",
        }

    if pairing_manager.is_paired:
        return {
            "ok": False,
            "error": "already_paired",
            "message": "This device is already paired. Unpair first.",
        }

    server = getattr(config, "server", None)
    convex_url = ""
    if server is not None:
        if getattr(server, "mode", "") == "self_hosted":
            self_hosted = getattr(server, "self_hosted", None)
            convex_url = (getattr(self_hosted, "url", "") or "").rstrip("/")
        else:
            cloud = getattr(server, "cloud", None)
            convex_url = (getattr(cloud, "url", "") or "").rstrip("/")
    if not convex_url:
        return {
            "ok": False,
            "error": "no_backend",
            "message": "Cloud backend URL is not configured.",
        }

    discovery = getattr(app, "discovery_service", None)
    short_id = config.agent.device_id[:6].lower()
    mdns_host = discovery.mdns_hostname if discovery else f"ados-{short_id}.local"
    api_key = pairing_manager.generate_api_key()
    board_obj = getattr(app, "board", None)
    board_name = getattr(board_obj, "name", None) or getattr(app, "board_name", "unknown")
    tier = getattr(board_obj, "tier", 0) or 0

    body = {
        "deviceId": config.agent.device_id,
        "pairingCode": cleaned,
        "apiKey": api_key,
        "name": getattr(config.agent, "name", "ADOS Agent"),
        "version": getattr(app, "version", ""),
        "board": board_name,
        "tier": tier,
        "mdnsHost": mdns_host,
        "localIp": "",
    }

    try:
        async with httpx.AsyncClient(timeout=10.0) as client:
            resp = await client.post(f"{convex_url}/pairing/register", json=body)
    except httpx.HTTPError as exc:
        log.warning("pairing_external_register_failed", error=str(exc))
        return {
            "ok": False,
            "error": "network",
            "message": f"Could not reach the cloud backend: {exc}",
        }

    if resp.status_code != 200:
        return {
            "ok": False,
            "error": "backend_error",
            "message": f"Backend returned {resp.status_code}.",
        }

    try:
        result = resp.json()
    except ValueError:
        return {"ok": False, "error": "bad_response", "message": "Backend response was not JSON."}

    if isinstance(result, dict) and result.get("error"):
        err = result["error"]
        msg_map = {
            "device_pending_with_different_code": (
                "This device is already pending a different code. Unpair first."
            ),
            "pairing_code_expired": (
                "The pairing code has expired. Generate a fresh one."
            ),
        }
        return {"ok": False, "error": err, "message": msg_map.get(err, err)}

    matched = bool(result.get("autoMatched") or result.get("alreadyClaimed"))
    if not matched:
        return {
            "ok": False,
            "error": "code_unknown",
            "message": (
                "No Mission Control session is waiting on that code yet. "
                "Ask Mission Control to generate a code and try again."
            ),
        }

    owner_id = result.get("userId") or result.get("ownerId") or "cloud"
    pairing_manager.claim(owner_id, api_key)

    if discovery is not None:
        try:
            from ados.core.profile import current_profile_and_role
            profile, role = current_profile_and_role(config)
            await discovery.update_txt(
                paired=True,
                owner=owner_id,
                profile=profile,
                role=role,
            )
        except Exception:
            log.debug("mdns_txt_update_failed_after_external_claim")

    return {
        "ok": True,
        "owner_id": owner_id,
        "paired_at": pairing_manager._state.get("paired_at"),
        "device_id": config.agent.device_id,
    }
