# SPDX-License-Identifier: GPL-3.0-only
# Copyright (C) 2026 Altnautica — ADOS Drone Agent
"""Feature manager — tracks enabled features and builds capabilities response."""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

from ados.core.config import ADOSConfig
from ados.core.logging import get_logger

log = get_logger("features")

STATE_DIR = Path("/var/ados/state")
FEATURES_FILE = STATE_DIR / "features.json"

# Built-in feature definitions.  Each key is a feature_id.
# "tier_required" is the minimum board tier needed to run the feature.
FEATURE_REGISTRY: dict[str, dict[str, Any]] = {
    "follow_me": {
        "name": "Follow Me",
        "description": "GPS or vision-based follow-me tracking",
        "category": "vision",
        "tier_required": 3,
        "requires_npu": False,
    },
    "precision_landing": {
        "name": "Precision Landing",
        "description": "AprilTag or visual marker precision landing",
        "category": "vision",
        "tier_required": 3,
        "requires_npu": False,
    },
    "object_detection": {
        "name": "Object Detection",
        "description": "Real-time object detection via NPU",
        "category": "vision",
        "tier_required": 3,
        "requires_npu": True,
    },
    "obstacle_avoidance": {
        "name": "Obstacle Avoidance",
        "description": "Depth-based obstacle detection and avoidance",
        "category": "vision",
        "tier_required": 3,
        "requires_npu": True,
    },
    "object_tracking": {
        "name": "Object Tracking",
        "description": "KCF or deep tracker for persistent object following",
        "category": "vision",
        "tier_required": 3,
        "requires_npu": True,
    },
    "crop_health": {
        "name": "Crop Health Mapping",
        "description": "Multispectral crop health analysis",
        "category": "agriculture",
        "tier_required": 3,
        "requires_npu": True,
    },
    "thermal_analysis": {
        "name": "Thermal Analysis",
        "description": "Thermal anomaly detection for inspection",
        "category": "inspection",
        "tier_required": 3,
        "requires_npu": True,
    },
    "quickshots": {
        "name": "QuickShots",
        "description": "Automated cinematic flight patterns (dronie, orbit, rocket)",
        "category": "flight",
        "tier_required": 2,
        "requires_npu": False,
    },
    "terrain_follow": {
        "name": "Terrain Following",
        "description": "Vision or rangefinder-based terrain following",
        "category": "flight",
        "tier_required": 2,
        "requires_npu": False,
    },
    "survey_capture": {
        "name": "Survey Capture",
        "description": "Automated survey image capture with overlap control",
        "category": "survey",
        "tier_required": 2,
        "requires_npu": False,
    },
}


class FeatureManager:
    """Manages feature flags and builds capabilities for the GCS."""

    def __init__(
        self,
        board_profile: dict[str, Any] | None,
        config: ADOSConfig,
    ) -> None:
        self._board = board_profile or {}
        self._config = config
        self._enabled: dict[str, bool] = {}
        self._active: dict[str, bool] = {}
        self._params: dict[str, dict[str, Any]] = {}
        self._load()

    # --- Persistence ---

    def _load(self) -> None:
        """Load enabled features from state file."""
        if FEATURES_FILE.exists():
            try:
                data = json.loads(FEATURES_FILE.read_text())
                self._enabled = data.get("enabled", {})
                self._params = data.get("params", {})
                log.info("features_loaded", count=len(self._enabled))
            except (json.JSONDecodeError, OSError) as exc:
                log.warning("features_load_failed", error=str(exc))
                self._enabled = {}
                self._params = {}
        else:
            self._enabled = {}
            self._params = {}

    def _save(self) -> None:
        """Persist enabled features to state file."""
        try:
            STATE_DIR.mkdir(parents=True, exist_ok=True)
            data = {
                "enabled": self._enabled,
                "params": self._params,
            }
            FEATURES_FILE.write_text(json.dumps(data, indent=2))
        except OSError as exc:
            log.error("features_save_failed", error=str(exc))

    # --- Feature control ---

    def enable(self, feature_id: str) -> dict[str, Any]:
        """Enable a feature by ID."""
        if feature_id not in FEATURE_REGISTRY:
            return {"status": "error", "message": f"Unknown feature: {feature_id}"}
        self._enabled[feature_id] = True
        self._save()
        log.info("feature_enabled", feature=feature_id)
        return {"status": "ok", "message": f"Feature {feature_id} enabled"}

    def disable(self, feature_id: str) -> dict[str, Any]:
        """Disable a feature by ID."""
        if feature_id not in FEATURE_REGISTRY:
            return {"status": "error", "message": f"Unknown feature: {feature_id}"}
        self._enabled.pop(feature_id, None)
        self._active.pop(feature_id, None)
        self._save()
        log.info("feature_disabled", feature=feature_id)
        return {"status": "ok", "message": f"Feature {feature_id} disabled"}

    def activate(self, feature_id: str) -> dict[str, Any]:
        """Activate a feature at runtime (start processing)."""
        if feature_id not in FEATURE_REGISTRY:
            return {"status": "error", "message": f"Unknown feature: {feature_id}"}
        if not self._enabled.get(feature_id):
            return {"status": "error", "message": f"Feature {feature_id} is not enabled"}
        self._active[feature_id] = True
        log.info("feature_activated", feature=feature_id)
        return {"status": "ok", "message": f"Feature {feature_id} activated"}

    def deactivate(self, feature_id: str) -> dict[str, Any]:
        """Deactivate a feature at runtime (stop processing)."""
        if feature_id not in FEATURE_REGISTRY:
            return {"status": "error", "message": f"Unknown feature: {feature_id}"}
        self._active.pop(feature_id, None)
        log.info("feature_deactivated", feature=feature_id)
        return {"status": "ok", "message": f"Feature {feature_id} deactivated"}

    def set_params(self, feature_id: str, params: dict[str, Any]) -> dict[str, Any]:
        """Update runtime parameters for a feature."""
        if feature_id not in FEATURE_REGISTRY:
            return {"status": "error", "message": f"Unknown feature: {feature_id}"}
        if feature_id not in self._params:
            self._params[feature_id] = {}
        self._params[feature_id].update(params)
        self._save()
        log.info("feature_params_updated", feature=feature_id, params=params)
        return {"status": "ok", "params": self._params[feature_id]}

    def get(self, feature_id: str) -> dict[str, Any] | None:
        """Get full info for a single feature."""
        defn = FEATURE_REGISTRY.get(feature_id)
        if not defn:
            return None
        return {
            "id": feature_id,
            **defn,
            "enabled": self._enabled.get(feature_id, False),
            "active": self._active.get(feature_id, False),
            "params": self._params.get(feature_id, {}),
        }

    # --- Camera scanning ---

    def _scan_cameras(self) -> list[dict[str, Any]]:
        """Scan /dev/video* for available camera devices."""
        cameras: list[dict[str, Any]] = []
        dev = Path("/dev")
        for vdev in sorted(dev.glob("video[0-9]*")):
            cam: dict[str, Any] = {
                "device": str(vdev),
                "name": vdev.name,
            }
            # Try to read device info from sysfs
            index = vdev.name.replace("video", "")
            sysfs = Path(f"/sys/class/video4linux/{vdev.name}/name")
            if sysfs.exists():
                try:
                    cam["name"] = sysfs.read_text().strip()
                except OSError:
                    pass
            cameras.append(cam)
        return cameras

    # --- Capabilities response ---

    def get_capabilities(self) -> dict[str, Any]:
        """Build the full capabilities response for the GCS."""
        compute = self._board.get("compute", {})
        npu_tops = compute.get("npu_tops", 0)
        npu_runtime = compute.get("npu_runtime")
        hw_encoder = compute.get("hw_encoder", [])
        hw_decoder = compute.get("hw_decoder", [])
        ram_mb = compute.get("ram_mb", 0)

        # Determine tier from board profile or config
        tier = self._board.get("default_tier") or self._board.get("profiles", {}).get("tier", "unknown")

        cameras = self._scan_cameras()

        # Vision capabilities
        vision_cfg = self._config.vision
        vision_info: dict[str, Any] = {
            "enabled": vision_cfg.enabled,
            "backend": vision_cfg.backend,
            "confidence_threshold": vision_cfg.confidence_threshold,
            "npu_tops": npu_tops,
            "npu_runtime": npu_runtime,
            "models_dir": vision_cfg.models_dir,
        }

        # Feature list with enabled/active state
        features: list[dict[str, Any]] = []
        for fid, defn in FEATURE_REGISTRY.items():
            features.append({
                "id": fid,
                "name": defn["name"],
                "description": defn["description"],
                "category": defn["category"],
                "tier_required": defn["tier_required"],
                "requires_npu": defn["requires_npu"],
                "enabled": self._enabled.get(fid, False),
                "active": self._active.get(fid, False),
                "params": self._params.get(fid, {}),
            })

        # Installed models
        models: list[dict[str, Any]] = []
        models_dir = Path(vision_cfg.models_dir)
        if models_dir.is_dir():
            for model_file in sorted(models_dir.iterdir()):
                if model_file.is_file() and model_file.suffix in (".rknn", ".tflite", ".onnx", ".engine"):
                    models.append({
                        "id": model_file.stem,
                        "filename": model_file.name,
                        "size_bytes": model_file.stat().st_size,
                        "format": model_file.suffix.lstrip("."),
                    })

        return {
            "tier": tier,
            "cameras": cameras,
            "compute": {
                "npu_tops": npu_tops,
                "npu_runtime": npu_runtime,
                "hw_encoder": hw_encoder,
                "hw_decoder": hw_decoder,
                "ram_mb": ram_mb,
            },
            "vision": vision_info,
            "models": models,
            "features": features,
        }
