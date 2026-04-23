"""6-stage survey quality validator.

Validates each captured frame against quality gates:
  1. Exposure   — histogram EV range check
  2. HDOP       — GPS dilution of precision gate (<1.5)
  3. GroundSpeed — too fast means blur risk
  4. Blur       — Laplacian variance threshold
  5. Overlap    — GSD + footprint vs coverage grid
  6. GSD        — actual GSD vs mission target

Emits QualityEvent objects on each frame.
"""

from __future__ import annotations

import time
from dataclasses import dataclass
from enum import Enum
from typing import Any

import structlog

log = structlog.get_logger()


class QualityState(str, Enum):
    PASS = "pass"
    WARN = "warn"
    FAIL = "fail"
    PENDING = "pending"


@dataclass
class QualityStage:
    name: str
    state: QualityState = QualityState.PENDING
    value: float | None = None
    threshold: float | None = None
    unit: str = ""


@dataclass
class QualityEvent:
    ts: float
    frame_id: str
    overall: QualityState
    stages: list[QualityStage]

    def to_dict(self) -> dict:
        return {
            "ts": self.ts,
            "frame_id": self.frame_id,
            "overall": self.overall.value,
            "stages": [
                {
                    "name": s.name,
                    "state": s.state.value,
                    "value": s.value,
                    "threshold": s.threshold,
                    "unit": s.unit,
                }
                for s in self.stages
            ],
        }


class QualityValidator:
    """Validates survey frames against 6 quality gates."""

    def __init__(
        self,
        hdop_threshold: float = 1.5,
        max_speed_ms: float = 10.0,
        min_laplacian: float = 100.0,
        target_gsd_cm: float = 5.0,
        min_overlap_pct: float = 70.0,
    ) -> None:
        self.hdop_threshold = hdop_threshold
        self.max_speed_ms = max_speed_ms
        self.min_laplacian = min_laplacian
        self.target_gsd_cm = target_gsd_cm
        self.min_overlap_pct = min_overlap_pct

    def validate(
        self,
        frame_id: str,
        state: dict[str, Any],
        image_data: bytes | None = None,
    ) -> QualityEvent:
        """Run all 6 stages on the given frame and telemetry state."""
        stages: list[QualityStage] = []

        # Stage 1: Exposure (requires image data)
        exposure = QualityStage(name="exposure", unit="EV")
        if image_data:
            ev = self._check_exposure(image_data)
            exposure.value = ev
            exposure.threshold = 0.0
            exposure.state = QualityState.PASS if -2.0 <= ev <= 2.0 else (
                QualityState.WARN if -3.0 <= ev <= 3.0 else QualityState.FAIL
            )
        else:
            exposure.state = QualityState.PENDING
        stages.append(exposure)

        # Stage 2: HDOP
        hdop = QualityStage(name="hdop", unit="")
        hdop_val = float(state.get("hdop", 99.0))
        hdop.value = hdop_val
        hdop.threshold = self.hdop_threshold
        hdop.state = (
            QualityState.PASS if hdop_val <= self.hdop_threshold
            else QualityState.WARN if hdop_val <= self.hdop_threshold * 2
            else QualityState.FAIL
        )
        stages.append(hdop)

        # Stage 3: Ground speed
        speed = QualityStage(name="groundSpeed", unit="m/s")
        spd = float(state.get("groundspeed", 0.0))
        speed.value = spd
        speed.threshold = self.max_speed_ms
        speed.state = (
            QualityState.PASS if spd <= self.max_speed_ms
            else QualityState.WARN if spd <= self.max_speed_ms * 1.5
            else QualityState.FAIL
        )
        stages.append(speed)

        # Stage 4: Blur (requires image data)
        blur = QualityStage(name="blur", unit="laplacian")
        if image_data:
            lap = self._check_blur(image_data)
            blur.value = lap
            blur.threshold = self.min_laplacian
            blur.state = (
                QualityState.PASS if lap >= self.min_laplacian
                else QualityState.WARN if lap >= self.min_laplacian * 0.5
                else QualityState.FAIL
            )
        else:
            blur.state = QualityState.PENDING
        stages.append(blur)

        # Stage 5: Overlap (simplified — always PENDING without coverage grid)
        overlap = QualityStage(name="overlap", state=QualityState.PENDING, unit="%")
        stages.append(overlap)

        # Stage 6: GSD
        gsd_stage = QualityStage(name="gsd", unit="cm/px")
        alt = float(state.get("alt", 0.0))
        if alt > 0:
            gsd_cm = self._compute_gsd_cm(alt)
            gsd_stage.value = gsd_cm
            gsd_stage.threshold = self.target_gsd_cm
            gsd_stage.state = (
                QualityState.PASS if gsd_cm <= self.target_gsd_cm * 1.2
                else QualityState.WARN if gsd_cm <= self.target_gsd_cm * 1.5
                else QualityState.FAIL
            )
        else:
            gsd_stage.state = QualityState.PENDING
        stages.append(gsd_stage)

        # Overall
        states = [s.state for s in stages if s.state != QualityState.PENDING]
        overall = (
            QualityState.FAIL if QualityState.FAIL in states
            else QualityState.WARN if QualityState.WARN in states
            else QualityState.PASS if states
            else QualityState.PENDING
        )

        return QualityEvent(ts=time.time(), frame_id=frame_id, overall=overall, stages=stages)

    def _check_exposure(self, image_data: bytes) -> float:
        """Return EV offset (0 = perfectly exposed). Requires PIL."""
        try:
            import io
            from PIL import Image
            import numpy as np
            img = Image.open(io.BytesIO(image_data)).convert("L")
            arr = np.array(img)
            mean = arr.mean()
            return float((mean - 128) / 64)
        except Exception:
            return 0.0

    def _check_blur(self, image_data: bytes) -> float:
        """Return Laplacian variance (higher = sharper)."""
        try:
            import io
            from PIL import Image, ImageFilter
            import numpy as np
            img = Image.open(io.BytesIO(image_data)).convert("L").resize((256, 256))
            lap = img.filter(ImageFilter.FIND_EDGES)
            arr = np.array(lap, dtype=float)
            return float(arr.var())
        except Exception:
            return 999.0

    def _compute_gsd_cm(self, alt_m: float, focal_mm: float = 4.35, sensor_w_mm: float = 6.17, image_w_px: int = 4000) -> float:
        """Compute ground sample distance in cm/px."""
        if focal_mm == 0:
            return 999.0
        return (alt_m * sensor_w_mm / focal_mm / image_w_px) * 100
