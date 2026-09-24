"""System-level service configuration (vision, logging, pairing, discovery, swarm, UI)."""

from __future__ import annotations

from typing import Literal

from pydantic import BaseModel, model_validator

from ados.core.paths import FLIGHT_LOGS_DIR, PAIRING_JSON


class VisionConfig(BaseModel):
    enabled: bool = False
    backend: str = "auto"  # auto, rknn, tensorrt, opencv_dnn, tflite
    confidence_threshold: float = 0.5
    models_dir: str = "/opt/ados/models/vision"
    models_cache_max_mb: int = 500
    registry_url: str = "https://raw.githubusercontent.com/altnautica/ADOSMissionControl/main/public/models/registry.json"


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
    max_size_mb: int = 50
    keep_count: int = 5
    flight_log_dir: str = str(FLIGHT_LOGS_DIR)
    store: LogStoreConfig = LogStoreConfig()


class PairingConfig(BaseModel):
    state_path: str = str(PAIRING_JSON)
    convex_url: str = ""  # Convex HTTP endpoint for cloud pairing
    beacon_interval: int = 30  # seconds
    heartbeat_interval: int = 60  # seconds
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

    cohesion: int = 40
    alignment: int = 60
    separation_gain: int = 150
    # Neighbours beyond this range contribute nothing to the flocking
    # terms; ``neighbors`` further caps how many of the nearest ones
    # are weighted, so a dense cluster cannot dominate the solution.
    radius_m: int = 30
    neighbors: int = 7


class SwarmSeparationConfig(BaseModel):
    """Collision-avoidance layer — the one swarm layer that is a safety
    function rather than a behaviour.

    ``radius_m`` is where repulsion starts; ``hard_m`` is where the
    horizontal solution is abandoned for a deterministic climb-and-hold.
    ``hard_m`` must stay below ``radius_m`` or repulsion never engages
    before the hard floor does.
    """

    radius_m: int = 8
    hard_m: int = 4

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
    bundle_position: int | None = None


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
    default_spacing: int = 10
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
