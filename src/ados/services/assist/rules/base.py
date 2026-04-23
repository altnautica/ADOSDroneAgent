"""Base Rule and Suggestion types for the Assist rules library."""

from __future__ import annotations

import secrets
import time
from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from ..correlator import ContextWindow


@dataclass
class Suggestion:
    id: str
    rule_id: str
    summary: str
    confidence: float  # 0.0 to 1.0
    safety_class: str  # read | safe_write | flight_action | destructive
    evidence: dict[str, Any] = field(default_factory=dict)
    proposed_repair_ids: list[str] = field(default_factory=list)
    created_at: float = field(default_factory=time.time)
    acknowledged_at: float | None = None
    dismissed_at: float | None = None

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "rule_id": self.rule_id,
            "summary": self.summary,
            "confidence": self.confidence,
            "safety_class": self.safety_class,
            "evidence": self.evidence,
            "proposed_repair_ids": self.proposed_repair_ids,
            "created_at": self.created_at,
            "acknowledged_at": self.acknowledged_at,
            "dismissed_at": self.dismissed_at,
        }


class Rule:
    """Base class for Assist rules."""
    id: str = "base"
    safety_class: str = "read"
    summary: str = "Base rule"

    def match(self, ctx: "ContextWindow") -> Suggestion | None:
        """Evaluate the rule against the context window.
        Returns a Suggestion if the rule fires, None otherwise.
        """
        return None

    def _suggestion(
        self,
        summary: str,
        confidence: float,
        evidence: dict[str, Any] | None = None,
        proposed_repairs: list[str] | None = None,
    ) -> Suggestion:
        return Suggestion(
            id=secrets.token_hex(6),
            rule_id=self.id,
            summary=summary,
            confidence=confidence,
            safety_class=self.safety_class,
            evidence=evidence or {},
            proposed_repair_ids=proposed_repairs or [],
        )


def load_all_rules() -> list[Rule]:
    """Load all available rules from the rules library."""
    from .wfb_rules import WfbFecBelowFloor, WfbRssiDrop, WfbLinkBudget
    from .fc_rules import FcPreArmWarning, FcFailsafeTrigger
    from .service_rules import ServiceCrashLoop, ServiceDegraded
    from .battery_rules import BatteryLow, BatteryVoltageSag
    from .network_rules import NetworkModemLost

    return [
        WfbFecBelowFloor(),
        WfbRssiDrop(),
        WfbLinkBudget(),
        FcPreArmWarning(),
        FcFailsafeTrigger(),
        ServiceCrashLoop(),
        ServiceDegraded(),
        BatteryLow(),
        BatteryVoltageSag(),
        NetworkModemLost(),
    ]
