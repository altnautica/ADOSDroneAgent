"""Capture rules engine for the World Model ingest pipeline.

YAML-based filter engine. First-match-wins. Up to 50 rules.
Rules decide whether to persist an observation, apply tags,
elevate the frame to full-res, or notify the GCS.

Rule format:
  - class: person           # required, supports '*' wildcard
    min_confidence: 0.7
    max_confidence: 1.0     # optional
    tags: [intruder]        # tags to apply when matched
    persist: true           # default true
    full_res: false         # store full-res frame, default false
    notify: false           # push GCS notification, default false
    deduplicate:
      within_seconds: 30    # skip if same class within N seconds
      within_meters: 20     # ... and within M meters

Default rule (when no rules match): persist if confidence >= 0.6.
"""

from __future__ import annotations

import re
import time
from collections import defaultdict
from typing import Any

import structlog
import yaml

log = structlog.get_logger()

MAX_RULES = 50

_DEFAULT_RULE = {
    "class": "*",
    "min_confidence": 0.6,
    "persist": True,
    "full_res": False,
    "notify": False,
    "tags": [],
}


class CaptureRule:
    def __init__(self, raw: dict[str, Any]) -> None:
        self.class_pattern: str = str(raw.get("class", "*"))
        self.min_confidence: float = float(raw.get("min_confidence", 0.0))
        self.max_confidence: float = float(raw.get("max_confidence", 1.0))
        self.tags: list[str] = list(raw.get("tags", []))
        self.persist: bool = bool(raw.get("persist", True))
        self.full_res: bool = bool(raw.get("full_res", False))
        self.notify: bool = bool(raw.get("notify", False))
        ded = raw.get("deduplicate", {})
        self.dedup_seconds: float = float(ded.get("within_seconds", 0))
        self.dedup_meters: float = float(ded.get("within_meters", 0))
        self._pattern = re.compile(
            "^" + re.escape(self.class_pattern).replace(r"\*", ".*") + "$"
        )

    def matches_class(self, cls: str) -> bool:
        return bool(self._pattern.match(cls))

    def matches_confidence(self, conf: float) -> bool:
        return self.min_confidence <= conf <= self.max_confidence


class CaptureRuleResult:
    def __init__(
        self,
        persist: bool = True,
        full_res: bool = False,
        notify: bool = False,
        tags: list[str] | None = None,
    ) -> None:
        self.persist = persist
        self.full_res = full_res
        self.notify = notify
        self.tags = tags or []


class DeduplicateIndex:
    """LRU-like index for deduplication checks."""

    def __init__(self) -> None:
        self._last: dict[str, tuple[float, float, float]] = {}

    def seen_recently(
        self,
        detect_class: str,
        lat: float | None,
        lon: float | None,
        within_seconds: float,
        within_meters: float,
    ) -> bool:
        key = detect_class
        if key not in self._last:
            return False
        last_ts, last_lat, last_lon = self._last[key]
        now = time.time()
        if now - last_ts > within_seconds:
            return False
        if within_meters > 0 and lat is not None and lon is not None:
            from math import radians, cos, sqrt
            dlat = radians(lat - last_lat) * 6371000
            dlon = radians(lon - last_lon) * 6371000 * cos(radians(lat))
            dist = sqrt(dlat ** 2 + dlon ** 2)
            if dist > within_meters:
                return False
        return True

    def mark_seen(self, detect_class: str, lat: float | None, lon: float | None) -> None:
        self._last[detect_class] = (time.time(), lat or 0.0, lon or 0.0)

    def clear(self) -> None:
        self._last.clear()


class RulesEngine:
    """Evaluates observations against the capture rules."""

    def __init__(self) -> None:
        self._rules: list[CaptureRule] = [CaptureRule(_DEFAULT_RULE)]
        self._dedup = DeduplicateIndex()
        self._match_counts: defaultdict[str, int] = defaultdict(int)

    def load_yaml(self, yaml_text: str) -> list[str]:
        """Parse and load rules from YAML. Returns list of validation errors."""
        errors: list[str] = []
        try:
            raw = yaml.safe_load(yaml_text)
        except yaml.YAMLError as e:
            return [f"YAML parse error: {e}"]

        if raw is None:
            self._rules = [CaptureRule(_DEFAULT_RULE)]
            return []

        if not isinstance(raw, list):
            return ["Rules must be a YAML list"]

        if len(raw) > MAX_RULES:
            return [f"Too many rules: {len(raw)} (max {MAX_RULES})"]

        new_rules = []
        for i, item in enumerate(raw):
            if not isinstance(item, dict):
                errors.append(f"Rule {i}: must be a dict")
                continue
            if "class" not in item:
                errors.append(f"Rule {i}: missing required 'class' field")
                continue
            new_rules.append(CaptureRule(item))

        if errors:
            return errors

        self._rules = new_rules or [CaptureRule(_DEFAULT_RULE)]
        self._dedup.clear()
        log.info("capture_rules_loaded", count=len(self._rules))
        return []

    def evaluate(
        self,
        detect_class: str,
        confidence: float,
        lat: float | None,
        lon: float | None,
    ) -> CaptureRuleResult:
        """Evaluate an observation against the rules. First match wins."""
        for rule in self._rules:
            if not rule.matches_class(detect_class):
                continue
            if not rule.matches_confidence(confidence):
                continue

            # Deduplication check
            if rule.dedup_seconds > 0:
                if self._dedup.seen_recently(
                    detect_class, lat, lon, rule.dedup_seconds, rule.dedup_meters
                ):
                    return CaptureRuleResult(persist=False)

            self._match_counts[detect_class] += 1
            self._dedup.mark_seen(detect_class, lat, lon)
            return CaptureRuleResult(
                persist=rule.persist,
                full_res=rule.full_res,
                notify=rule.notify,
                tags=rule.tags,
            )

        # No match: default behavior
        if confidence >= 0.6:
            self._dedup.mark_seen(detect_class, lat, lon)
            return CaptureRuleResult(persist=True)
        return CaptureRuleResult(persist=False)

    def dry_run(
        self,
        detections: list[dict[str, Any]],
    ) -> list[dict[str, Any]]:
        """Dry-run detections through rules. Returns per-detection results."""
        results = []
        for d in detections:
            result = self.evaluate(
                d.get("class", ""),
                float(d.get("confidence", 0)),
                d.get("lat"),
                d.get("lon"),
            )
            results.append({
                "class": d.get("class"),
                "confidence": d.get("confidence"),
                "persist": result.persist,
                "full_res": result.full_res,
                "tags": result.tags,
            })
        return results

    def stats(self) -> dict[str, int]:
        return dict(self._match_counts)

    def rules_yaml(self) -> str:
        """Serialize current rules back to YAML for the GCS editor."""
        import yaml as _yaml
        rules_data = []
        for r in self._rules:
            d: dict[str, Any] = {"class": r.class_pattern, "min_confidence": r.min_confidence}
            if r.tags:
                d["tags"] = r.tags
            if r.full_res:
                d["full_res"] = True
            if r.notify:
                d["notify"] = True
            if not r.persist:
                d["persist"] = False
            if r.dedup_seconds > 0:
                d["deduplicate"] = {"within_seconds": r.dedup_seconds, "within_meters": r.dedup_meters}
            rules_data.append(d)
        return _yaml.dump(rules_data, default_flow_style=False, allow_unicode=True)
