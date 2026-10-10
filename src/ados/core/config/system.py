"""System-level service configuration (vision, logging, pairing, discovery, swarm, UI)."""

from __future__ import annotations

from typing import Literal

from pydantic import BaseModel, Field, model_validator

from ados.core.paths import FLIGHT_LOGS_DIR, PAIRING_JSON

from ._bounds import I64_MAX, I64_MIN, U8_MAX, U32_MAX

# The inference backends the native vision engine selects between. A closed
# set: any other name used to fall through to the mock engine silently.
VisionBackend = Literal["auto", "rknn", "hailo", "onnx", "mock"]


# The vision sub-models default the fields the native engine requires
# (`model_id`, `model_path`, a camera's `id`) to empty, so a partly written
# block never stops the Python services from loading; the engine refuses such
# an entry itself.
class VisionCameraEntry(BaseModel):
    """One explicitly configured camera; an empty list uses HAL discovery."""

    id: str = ""
    # "tap" reads the video pipeline's frame tap; "capture" opens the device.
    kind: str = "tap"
    # For "tap": the frame socket; unset is `<socket_dir>/vision-tap-<id>.sock`.
    tap_socket: str | None = None
    # For "capture": the device path; unset is resolved by HAL discovery.
    device_path: str | None = None


class VisionDetectorConfig(BaseModel):
    """The detector the engine runs on every frame for `vision.detection`."""

    model_id: str = ""
    model_path: str = ""
    input_width: int = Field(default=640, ge=0, le=U32_MAX)
    input_height: int = Field(default=640, ge=0, le=U32_MAX)
    # Output-head layout: `yolo8` (empty is yolo8) or `yolo5`.
    head: str = ""
    class_labels: list[str] = Field(default_factory=list)
    # Target inference rate; unset runs on every captured frame.
    rate_hz: float | None = None
    # Higher runs first when several models are due at once.
    priority: int | None = Field(default=None, ge=0, le=U8_MAX)


class VisionReidConfig(BaseModel):
    """The appearance (re-id) model loaded beside the detector."""

    model_id: str = ""
    model_path: str = ""
    input_width: int = Field(default=128, ge=0, le=U32_MAX)
    input_height: int = Field(default=256, ge=0, le=U32_MAX)


class VisionConfig(BaseModel):
    enabled: bool = False
    backend: VisionBackend = "auto"
    # Bench aid for the mock backend: emit one synthetic moving `person`
    # detection so detection consumers can be exercised without a model.
    mock_target: bool = False
    confidence_threshold: float = 0.5
    models_dir: str = "/opt/ados/models/vision"
    models_cache_max_mb: int = Field(default=500, ge=0, le=U32_MAX)
    registry_url: str = "https://raw.githubusercontent.com/altnautica/ADOSMissionControl/main/public/models/registry.json"
    # Overrides `agent.profile` for the engine; unset follows it.
    profile: str | None = None
    socket_dir: str = "/run/ados"
    cameras: list[VisionCameraEntry] = Field(default_factory=list)
    downscale_width: int = Field(default=640, ge=0, le=U32_MAX)
    downscale_height: int = Field(default=480, ge=0, le=U32_MAX)
    slot_count: int = Field(default=4, ge=0, le=U32_MAX)
    shm_budget_mb: int = Field(default=64, ge=0, le=U32_MAX)
    tracker_enabled: bool = False
    reid_enabled: bool = False
    reid_model_id: str | None = None
    designate_camera: str | None = None
    detector: VisionDetectorConfig | None = None
    reid: VisionReidConfig | None = None


class LogStoreConfig(BaseModel):
    """The durable local logging and telemetry store — the black box.

    **Off by default, deliberately.** Measured on a drone, the node wrote
    904 KB/s with the store running and 49 KB/s with it stopped: the store is
    roughly 96% of everything reaching the card, and the largest single lump of
    space it occupies. Cards were filling and corrupting, and nodes were being
    reflashed, largely because of it.

    Turning it off is a real capability regression and is not dressed up as
    anything else: while it is off the node has no durable flight recorder, and
    ``journalctl`` is the log of record. That is why the persistent journal is
    kept — with the store gone it is the only thing that survives a reboot.

    Turning it back on is this one key plus a restart, never a reinstall: the
    binary is installed either way and the installer reconciles the unit to
    match on the next run.
    """

    enabled: bool = False


class LoggingConfig(BaseModel):
    level: str = "info"
    max_size_mb: int = Field(default=50, ge=0, le=U32_MAX)
    keep_count: int = Field(default=5, ge=0, le=U32_MAX)
    flight_log_dir: str = str(FLIGHT_LOGS_DIR)
    store: LogStoreConfig = LogStoreConfig()


class PairingConfig(BaseModel):
    state_path: str = str(PAIRING_JSON)
    convex_url: str = ""  # Convex HTTP endpoint for cloud pairing
    beacon_interval: int = Field(default=30, ge=0, le=U32_MAX)  # seconds
    heartbeat_interval: int = Field(default=60, ge=0, le=U32_MAX)  # seconds
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


class WifiDirectConfig(BaseModel):
    enabled: bool = False
    interface: str = ""


class SwarmFlockConfig(BaseModel):
    """Olfati-Saber alpha-lattice flocking weights.

    The three gains are integer PERCENTAGES of the underlying float
    weight (``cohesion = 40`` means 0.40): the GCS config primitives
    carry no float field, so expressing them as bounded integers keeps
    one validation path on both sides. A consumer divides by 100.
    """

    cohesion: int = Field(default=40, ge=I64_MIN, le=I64_MAX)
    alignment: int = Field(default=60, ge=I64_MIN, le=I64_MAX)
    separation_gain: int = Field(default=150, ge=I64_MIN, le=I64_MAX)
    # Neighbours beyond this range contribute nothing to the flocking
    # terms; ``neighbors`` further caps how many of the nearest ones
    # are weighted, so a dense cluster cannot dominate the solution.
    radius_m: int = Field(default=30, ge=I64_MIN, le=I64_MAX)
    neighbors: int = Field(default=7, ge=I64_MIN, le=I64_MAX)


class SwarmSeparationConfig(BaseModel):
    """Collision-avoidance layer — the one swarm layer that is a safety
    function rather than a behaviour.

    ``radius_m`` is where repulsion starts; ``hard_m`` is where the
    horizontal solution is abandoned for a deterministic climb-and-hold.
    ``hard_m`` must stay below ``radius_m`` or repulsion never engages
    before the hard floor does.
    """

    radius_m: int = Field(default=8, ge=I64_MIN, le=I64_MAX)
    hard_m: int = Field(default=4, ge=I64_MIN, le=I64_MAX)

    @model_validator(mode="after")
    def _hard_below_radius(self) -> SwarmSeparationConfig:
        if self.hard_m >= self.radius_m:
            raise ValueError(
                "swarm.separation.hard_m must be less than "
                "swarm.separation.radius_m"
            )
        return self


class SwarmTasksConfig(BaseModel):
    """Consensus-based task allocation (CBBA) participation.

    ``enabled`` is the operator's switch. The two assignment fields are
    AGENT-WRITTEN status mirrors persisted into the config tree (the
    same pattern ``video.wfb.paired_with_device_id`` uses), so a surface
    reading the config can show the current assignment without a second
    transport. Both stay ``None`` until a swarm runtime writes them —
    never a fabricated default.
    """

    enabled: bool = False
    assigned_task_id: str | None = None
    bundle_position: int | None = Field(default=None, ge=I64_MIN, le=I64_MAX)


class SwarmConfig(BaseModel):
    enabled: bool = False
    wifi_direct: WifiDirectConfig = WifiDirectConfig()
    role: str = "auto"
    # Closed set, matching the built-in formation generators. A free
    # string here was typo-prone and silently produced no formation at
    # all, so the model rejects an unknown name at load time.
    default_formation: Literal[
        "line", "column", "wedge", "grid", "circle"
    ] = "line"
    default_spacing: int = Field(default=10, ge=I64_MIN, le=I64_MAX)
    # The operator-commandable behaviour mode, set per node or fanned
    # across a selection from the GCS. Hard separation and operator
    # direct command are precedence LEVELS the runtime arbitrates into,
    # not modes anyone commands, so neither is a value here.
    mode: Literal["hold", "flocking", "formation"] = "hold"
    flock: SwarmFlockConfig = SwarmFlockConfig()
    separation: SwarmSeparationConfig = SwarmSeparationConfig()
    tasks: SwarmTasksConfig = SwarmTasksConfig()


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
