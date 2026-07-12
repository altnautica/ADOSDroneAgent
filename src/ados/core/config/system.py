"""System-level service configuration (vision, atlas, logging, pairing, discovery, swarm, UI)."""

from __future__ import annotations

from typing import Literal

from pydantic import BaseModel

from ados.core.paths import FLIGHT_LOGS_DIR, PAIRING_JSON


class VisionConfig(BaseModel):
    enabled: bool = False
    backend: str = "auto"  # auto, rknn, tensorrt, opencv_dnn, tflite
    confidence_threshold: float = 0.5
    models_dir: str = "/opt/ados/models/vision"
    models_cache_max_mb: int = 500
    registry_url: str = "https://raw.githubusercontent.com/altnautica/ADOSMissionControl/main/public/models/registry.json"
    auto_download: bool = True


class AtlasCameraConfig(BaseModel):
    """One camera on the world-model rig (mirrors the Rust capture-core shape).

    ``enabled`` gates whether the camera's frames are captured at all;
    ``reconstruct`` is the per-camera hint about whether the stream feeds the
    world-model reconstruction (a camera may be captured for situational video
    yet excluded from the splat).
    """

    id: str
    # Matches the Rust CameraRole variants exactly so an invalid value is
    # rejected here (at the write boundary) instead of silently disabling the
    # Rust capture service when its strict serde enum fails to parse.
    role: Literal["primary", "aux", "down", "left", "right", "back", "up"] = "primary"
    enabled: bool = True
    reconstruct: bool = True


class AtlasSelectionParams(BaseModel):
    """Keyframe-selection thresholds (mirrors the Rust defaults)."""

    min_translation_m: float = 0.5
    min_rotation_rad: float = 0.26  # ~15 degrees
    max_interval_ms: int = 2000
    max_keyframes: int = 0  # 0 = unlimited; a session-wide cap on selected keyframes


class AtlasIntrinsicsOverride(BaseModel):
    """A per-camera calibrated pinhole. Absent, the capture service derives an
    uncalibrated pinhole from the frame size and the field of view."""

    fx: float
    fy: float
    cx: float
    cy: float
    distortion_model: str | None = None
    distortion_params: list[float] = []


class AtlasConfig(BaseModel):
    """ADOS Atlas world-model configuration.

    Default off (``enabled``): a fresh agent runs no Atlas capture, no
    compute-node services, and no perception offload until this is enabled. One
    flag keeps the whole program inert, the same shape as ``VisionConfig``. The
    remaining fields mirror the Rust ``atlas:`` block the native capture service
    (``ados-atlas``) reads, so the persisted YAML round-trips identically through
    both halves rather than dropping fields the Rust side relies on.
    """

    enabled: bool = False
    socket_dir: str = "/run/ados"
    cameras: list[AtlasCameraConfig] = []
    # capture_profile and pose_tier match the Rust CaptureProfile / PoseTierConfig
    # variants exactly (strict serde enums on the Rust side); Literal rejects an
    # invalid value here rather than letting it silently disable the Rust service.
    capture_profile: Literal["orbit", "lawnmower", "freeform", "inspection"] = "freeform"
    # The default reconstruction detail level, in Brush training steps, set from
    # the drone tab. Consumed by the GCS at reconstruct-submit time (mirrors the
    # Rust atlas: block so the YAML round-trips through both halves).
    reconstruct_steps: int = 30000
    selection: AtlasSelectionParams = AtlasSelectionParams()
    pose_tier: Literal["auto", "local", "offload", "hybrid"] = "auto"
    hfov_deg: float = 70.0
    intrinsics: dict[str, AtlasIntrinsicsOverride] = {}


class PerceptionOffloadConfig(BaseModel):
    """Drone-side perception offload: where the heavy detector runs.

    ``enabled`` is a tri-state: ``auto`` (offload when the board is NPU-less and a
    workstation is reachable on the LAN — the default), ``on`` (force offload),
    ``off`` (never offload). ``compute_node_addr`` pins a specific workstation
    (``host:port``); empty means auto-discover over mDNS. Mirrors the Rust
    ``perception.offload`` block the reconciler reads.
    """

    enabled: Literal["auto", "on", "off"] = "auto"
    compute_node_addr: str | None = None


class PerceptionServingConfig(BaseModel):
    """Workstation-side offload serving: whether this node runs detectors for
    other drones and which one by default.

    ``enabled`` tri-state: ``auto`` (auto-accept + serve LAN offload — the
    default), ``on`` (force serving), ``off`` (never serve). ``detector_model``
    picks the served detector by model id; empty means the daemon's default.
    """

    enabled: Literal["auto", "on", "off"] = "auto"
    detector_model: str | None = None


class PerceptionConfig(BaseModel):
    """Two-tier perception execution config (mirrors the Rust ``perception:``
    block). ``offload`` is read on a drone, ``serving`` on a workstation; both
    default so a fresh agent needs no setup — a no-NPU drone + a workstation on
    one LAN offload hands-free."""

    offload: PerceptionOffloadConfig = PerceptionOffloadConfig()
    serving: PerceptionServingConfig = PerceptionServingConfig()


class LoggingConfig(BaseModel):
    level: str = "info"
    max_size_mb: int = 50
    keep_count: int = 5
    flight_log_dir: str = str(FLIGHT_LOGS_DIR)


class PairingConfig(BaseModel):
    state_path: str = str(PAIRING_JSON)
    convex_url: str = ""  # Convex HTTP endpoint for cloud pairing
    beacon_interval: int = 30  # seconds
    heartbeat_interval: int = 60  # seconds
    single_process_cloud_enabled: bool = False
    code_ttl: int = 900  # 15 minutes
    # Cloud pair beacon publishes the unpaired agent's short-lived
    # pair code to ``convex_url`` so a GCS reached from any network
    # (e.g. command.altnautica.com) can claim by code. Loop runs only
    # while unpaired and gates on a non-empty ``convex_url`` — air-gap
    # operators get a clean opt-out by setting ``server.mode = "local"``
    # which clears the URL and stops every cloud-touching task at the
    # same gate.
    beacon_enabled: bool = True


class DiscoveryConfig(BaseModel):
    mdns_enabled: bool = True
    service_type: str = "_ados._tcp.local."


class LoraConfig(BaseModel):
    interface: str = ""
    frequency: int = 915000000
    bandwidth: int = 125000
    spreading_factor: int = 7


class WifiDirectConfig(BaseModel):
    enabled: bool = False
    interface: str = ""


class SwarmConfig(BaseModel):
    enabled: bool = False
    lora: LoraConfig = LoraConfig()
    wifi_direct: WifiDirectConfig = WifiDirectConfig()
    role: str = "auto"
    default_formation: str = "line"
    default_spacing: int = 10


# Mirrors `ados.setup.models.UiConfig` shape so the persisted YAML
# round-trips through both the setup-facade payload model and the
# top-level config model. Defined inline (not imported from setup) so
# `ados.core.config` stays free of inbound dependencies on the setup
# package and the import graph remains a tree, not a cycle.
class UiConfig(BaseModel):
    """UI presentation settings persisted on disk.

    `theme` drives the SPI LCD dashboard palette. The native display
    service reads it on every render tick, so a flip from `dark` to
    `light` takes effect immediately without a service restart.
    """

    theme: Literal["dark", "light"] = "dark"
