"""Pairing manager for ADOS Drone Agent."""

from __future__ import annotations

import json
import secrets
import time
from pathlib import Path
from typing import Optional

from ados.core.logging import get_logger

log = get_logger("pairing")

# Safe charset: no ambiguous chars (0/O/1/I/L)
SAFE_CHARSET = "ABCDEFGHJKMNPQRSTUVWXYZ23456789"
CODE_LENGTH = 6
CODE_TTL = 900  # 15 minutes
PAIRING_STATE_PATH = "/etc/ados/pairing.json"


class PairingManager:
    """Manages pairing state, code generation, and API key validation."""

    def __init__(self, state_path: str = PAIRING_STATE_PATH):
        self._state_path = Path(state_path)
        self._state: dict = {}
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

    def _save_state(self) -> None:
        self._state_path.parent.mkdir(parents=True, exist_ok=True)
        tmp = self._state_path.with_suffix(".tmp")
        tmp.write_text(json.dumps(self._state, indent=2))
        tmp.rename(self._state_path)
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
        return self._state.get("paired", False)

    @property
    def api_key(self) -> Optional[str]:
        return self._state.get("api_key") if self.is_paired else None

    @property
    def owner_id(self) -> Optional[str]:
        return self._state.get("owner_id") if self.is_paired else None

    def get_or_create_code(self) -> str:
        """Get current pairing code, or generate a new one if expired."""
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

    def claim(self, user_id: str, api_key: Optional[str] = None) -> str:
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
        if not self.is_paired:
            return True  # When unpaired, all access is open
        return key == self._state.get("api_key")

    def get_info(self) -> dict:
        """Get pairing info for the /pairing/info endpoint."""
        if self.is_paired:
            return {
                "paired": True,
                "owner_id": self._state.get("owner_id"),
                "paired_at": self._state.get("paired_at"),
            }
        return {
            "paired": False,
            "pairing_code": self.get_or_create_code(),
        }
