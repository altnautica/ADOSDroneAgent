"""Suggestion emitter for the Assist service.

Runs all registered rules against the current context window,
deduplicates by rule_id (only one active suggestion per rule at a time),
ranks by confidence, and stores to the suggestion queue.
"""

from __future__ import annotations

import time
from collections import OrderedDict
from typing import Any

import structlog

from .rules.base import Rule, Suggestion

log = structlog.get_logger()

MAX_ACTIVE_SUGGESTIONS = 20
DEDUP_TTL_SECONDS = 300  # 5 minutes


class SuggestionEmitter:
    """Evaluates rules and maintains the active suggestion queue."""

    def __init__(self, rules: list[Rule]) -> None:
        self._rules = rules
        self._active: OrderedDict[str, Suggestion] = OrderedDict()
        self._disabled_rules: set[str] = set()

    def run_pass(self, ctx) -> list[Suggestion]:
        """Run all rules against the context window. Returns new suggestions."""
        new_suggestions: list[Suggestion] = []
        now = time.time()

        for rule in self._rules:
            if rule.id in self._disabled_rules:
                continue
            try:
                suggestion = rule.match(ctx)
                if suggestion is None:
                    continue
                # Dedup: skip if same rule fired recently
                existing = self._active.get(rule.id)
                if existing and (now - existing.created_at) < DEDUP_TTL_SECONDS:
                    continue

                self._active[rule.id] = suggestion
                new_suggestions.append(suggestion)
                log.info(
                    "assist_suggestion_fired",
                    rule_id=rule.id,
                    confidence=suggestion.confidence,
                    summary=suggestion.summary[:80],
                )
            except Exception as e:
                log.warning("assist_rule_error", rule_id=rule.id, error=str(e))
                self._disabled_rules.add(rule.id)

        # Evict old suggestions
        if len(self._active) > MAX_ACTIVE_SUGGESTIONS:
            oldest_keys = list(self._active.keys())[: len(self._active) - MAX_ACTIVE_SUGGESTIONS]
            for k in oldest_keys:
                del self._active[k]

        return new_suggestions

    def list_active(self) -> list[Suggestion]:
        """Return active suggestions sorted by confidence (descending)."""
        return sorted(self._active.values(), key=lambda s: s.confidence, reverse=True)

    def acknowledge(self, suggestion_id: str) -> bool:
        for s in self._active.values():
            if s.id == suggestion_id:
                s.acknowledged_at = time.time()
                return True
        return False

    def dismiss(self, suggestion_id: str) -> bool:
        for rule_id, s in list(self._active.items()):
            if s.id == suggestion_id:
                s.dismissed_at = time.time()
                del self._active[rule_id]
                return True
        return False
