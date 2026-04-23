"""MCP gate layer — enforces safety class checks on tool calls.

The gate sits between the MCP protocol handler and the tool executors.
Every tool call passes through the gate in this order:

  1. Token extraction (from Authorization: Bearer <secret>)
  2. Token lookup and expiry check
  3. Scope check against the tool's safety_class
  4. Operator-present check (flight_action class)
  5. Typed-confirm check (destructive class)
  6. Audit pre-write
  7. Execute handler
  8. Audit finalize with outcome + latency

Safety classes:
  read          — any token with 'read' scope. Audit sampled 1/N.
  safe_write    — requires 'safe_write' scope. Full audit.
  flight_action — requires 'flight_action' scope AND operator-present OR sim-mode.
  destructive   — requires 'destructive' scope AND typed confirm signed within 60s.

Operator-present is a global flag managed by the GCS heartbeat.
Typed confirms are one-time tokens minted by the GCS confirm modal.
"""

from __future__ import annotations

import hashlib
import hmac
import time
from dataclasses import dataclass, field
from typing import Any

import structlog

from .tokens import McpToken, TokenStore

log = structlog.get_logger()


# Safety class definitions
SAFETY_CLASSES = frozenset({"read", "safe_write", "flight_action", "destructive"})

# Required scope per safety class
REQUIRED_SCOPE: dict[str, str] = {
    "read": "read",
    "safe_write": "safe_write",
    "flight_action": "flight_action",
    "destructive": "destructive",
}

# Safety class per tool name
TOOL_SAFETY: dict[str, str] = {
    # flight
    "flight.arm": "flight_action",
    "flight.disarm": "flight_action",
    "flight.takeoff": "flight_action",
    "flight.land": "flight_action",
    "flight.rtl": "flight_action",
    "flight.set_mode": "flight_action",
    "flight.goto": "flight_action",
    "flight.orbit": "flight_action",
    "flight.pause": "flight_action",
    "flight.resume": "flight_action",
    "flight.emergency_stop": "flight_action",
    # telemetry
    "telemetry.snapshot": "read",
    "telemetry.battery": "read",
    "telemetry.gps": "read",
    "telemetry.attitude": "read",
    "telemetry.history": "read",
    # params
    "params.list": "read",
    "params.get": "read",
    "params.set": "safe_write",
    "params.diff": "read",
    "params.save_to_flash": "safe_write",
    "params.reset_to_default": "safe_write",
    "params.reset_all_to_default": "destructive",
    # config
    "config.get": "read",
    "config.set": "safe_write",
    "config.validate": "read",
    "config.apply": "safe_write",
    "config.reload": "safe_write",
    # files
    "files.list": "read",
    "files.read": "read",
    "files.write": "safe_write",
    "files.delete": "destructive",
    "files.stat": "read",
    "files.move": "safe_write",
    # services
    "services.list": "read",
    "services.status": "read",
    "services.start": "safe_write",
    "services.stop": "flight_action",
    "services.restart": "flight_action",
    "services.logs": "read",
    # video
    "video.status": "read",
    "video.snapshot": "read",
    "video.record_start": "safe_write",
    "video.record_stop": "safe_write",
    "video.set_bitrate": "safe_write",
    "video.switch_camera": "safe_write",
    # vision
    "vision.list_models": "read",
    "vision.set_model": "safe_write",
    "vision.detect_now": "read",
    "vision.start_tracker": "safe_write",
    "vision.stop_tracker": "safe_write",
    # memory
    "memory.observations.list": "read",
    "memory.observations.get": "read",
    "memory.observations.search": "read",
    "memory.observations.tag": "safe_write",
    "memory.entities.list": "read",
    "memory.entities.get": "read",
    "memory.entities.merge": "safe_write",
    "memory.entities.rename": "safe_write",
    "memory.places.list": "read",
    "memory.places.get": "read",
    "memory.place.add": "safe_write",
    "memory.flights.list": "read",
    "memory.flights.get": "read",
    "memory.frames.search_embedding": "read",
    "memory.diff": "read",
    "memory.snapshot": "safe_write",
    "memory.sync_to_cloud": "safe_write",
    # mission
    "mission.upload": "safe_write",
    "mission.download": "read",
    "mission.start": "flight_action",
    "mission.clear": "safe_write",
    "mission.current_item": "read",
    "mission.set_current_item": "flight_action",
    # ota
    "ota.check": "read",
    "ota.install": "safe_write",
    "ota.rollback": "destructive",
    # system
    "system.reboot": "destructive",
    "system.shutdown": "destructive",
    "system.reset_factory": "destructive",
    "system.time": "read",
    # ros
    "ros.status": "read",
    "ros.list_nodes": "read",
    "ros.list_topics": "read",
    "ros.start_bag": "safe_write",
    "ros.stop_bag": "safe_write",
    # agent
    "agent.health": "read",
    "agent.version": "read",
    "agent.tier": "read",
    "agent.board": "read",
    "agent.capabilities": "read",
    "agent.identity": "read",
    "agent.uptime": "read",
    "agent.feature_flags": "read",
    # assist
    "assist.diagnose": "read",
    "assist.suggest_for": "read",
    "assist.subscribe_diagnostics": "read",
    "assist.acknowledge_suggestion": "safe_write",
    "assist.dismiss_suggestion": "safe_write",
    "assist.get_status": "read",
    "assist.set_features": "safe_write",
    "assist.set_scope": "safe_write",
    "repair.list_pending": "read",
    "repair.approve": "flight_action",
    "repair.execute": "flight_action",
    "repair.rollback": "safe_write",
    "repair.audit_log": "read",
    "repair.cancel": "safe_write",
    "pr.draft": "safe_write",
    "pr.preview_diff": "read",
    "pr.list_drafts": "read",
    "pr.push": "safe_write",
    "pr.cancel": "safe_write",
    "pr.list_open": "read",
    "setup.start_wizard": "read",
    "setup.next_step": "safe_write",
    "setup.previous_step": "safe_write",
    "setup.complete": "safe_write",
    "setup.list_wizards": "read",
    "fleet.detect_patterns": "read",
    "fleet.suggest_fix_for_pattern": "read",
    "fleet.list_patterns": "read",
    "fleet.get_pattern_detail": "read",
    "assist.opt_in.enable_feature": "safe_write",
    "assist.opt_in.disable_feature": "safe_write",
    "assist.opt_in.set_safety_scope": "safe_write",
    "assist.opt_in.list_enabled": "read",
}


@dataclass
class PendingConfirm:
    """A one-time typed confirm record created by the GCS confirm modal."""
    confirm_id: str
    tool_name: str
    typed_phrase: str
    phrase_hash: str
    created_at: float
    ttl: float = 60.0

    @property
    def expired(self) -> bool:
        return time.time() > self.created_at + self.ttl


class GateStore:
    """In-memory store for pending typed confirms."""

    def __init__(self) -> None:
        self._confirms: dict[str, PendingConfirm] = {}

    def create_confirm(self, tool_name: str, typed_phrase: str) -> PendingConfirm:
        """Create a new confirm record and return it."""
        import secrets
        confirm_id = secrets.token_hex(8)
        phrase_hash = hashlib.sha256(typed_phrase.encode()).hexdigest()
        confirm = PendingConfirm(
            confirm_id=confirm_id,
            tool_name=tool_name,
            typed_phrase=typed_phrase,
            phrase_hash=phrase_hash,
            created_at=time.time(),
        )
        self._confirms[confirm_id] = confirm
        return confirm

    def consume_confirm(self, confirm_id: str, tool_name: str, typed_phrase: str) -> bool:
        """Verify and consume a confirm token. Returns True on success."""
        confirm = self._confirms.get(confirm_id)
        if not confirm:
            return False
        if confirm.expired:
            del self._confirms[confirm_id]
            return False
        phrase_hash = hashlib.sha256(typed_phrase.encode()).hexdigest()
        if confirm.tool_name != tool_name:
            return False
        if not hmac.compare_digest(confirm.phrase_hash, phrase_hash):
            return False
        del self._confirms[confirm_id]
        return True

    def purge_expired(self) -> None:
        self._confirms = {k: v for k, v in self._confirms.items() if not v.expired}


class GateResult:
    PASS = "pass"
    GATE_BLOCKED = "gate_blocked"

    def __init__(self, decision: str, reason: str = "", token: McpToken | None = None) -> None:
        self.decision = decision
        self.reason = reason
        self.token = token

    @property
    def passed(self) -> bool:
        return self.decision == self.PASS


class Gate:
    """Enforces safety class checks on tool calls."""

    REQUIRES_OPERATOR_PRESENT_CONFIRMATION_WINDOW = 30.0  # seconds

    def __init__(
        self,
        token_store: TokenStore,
        gate_store: GateStore,
        operator_present_getter,  # callable() -> bool
    ) -> None:
        self._tokens = token_store
        self._confirms = gate_store
        self._operator_present = operator_present_getter

    def tool_safety_class(self, tool_name: str) -> str:
        return TOOL_SAFETY.get(tool_name, "read")

    def check(
        self,
        bearer: str | None,
        tool_name: str,
        confirm_id: str | None = None,
        typed_phrase: str | None = None,
        sim_mode: bool = False,
    ) -> GateResult:
        """Run the gate check. Returns GateResult with decision."""

        # 1. Token extraction + auth
        if not bearer:
            return GateResult(GateResult.GATE_BLOCKED, "No bearer token provided")

        token = self._tokens.authenticate(bearer)
        if not token:
            return GateResult(GateResult.GATE_BLOCKED, "Invalid, expired, or revoked token")

        safety = self.tool_safety_class(tool_name)
        required_scope = REQUIRED_SCOPE.get(safety, "read")

        # 2. Scope check
        if not token.has_scope(required_scope):
            return GateResult(
                GateResult.GATE_BLOCKED,
                f"Token missing required scope '{required_scope}' for {safety} tool '{tool_name}'",
            )

        # 3. Operator-present check (flight_action)
        if safety == "flight_action" and not sim_mode:
            if not self._operator_present():
                return GateResult(
                    GateResult.GATE_BLOCKED,
                    f"Tool '{tool_name}' requires operator-present to be enabled. "
                    "Enable the Operator Present toggle in the GCS.",
                )

        # 4. Typed confirm check (destructive)
        if safety == "destructive":
            if not confirm_id or not typed_phrase:
                return GateResult(
                    GateResult.GATE_BLOCKED,
                    f"Tool '{tool_name}' is destructive and requires a typed confirmation. "
                    "Use the GCS confirm modal to authorize this action.",
                )
            if not self._confirms.consume_confirm(confirm_id, tool_name, typed_phrase):
                return GateResult(
                    GateResult.GATE_BLOCKED,
                    f"Typed confirmation for '{tool_name}' invalid, expired, or already used.",
                )

        return GateResult(GateResult.PASS, token=token)
