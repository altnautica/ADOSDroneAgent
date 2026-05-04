"""Pydantic models for the universal setup surface."""

from __future__ import annotations

from typing import Literal

from pydantic import BaseModel, Field

SetupStepState = Literal[
    "complete", "needs_action", "optional", "blocked", "not_applicable"
]


class SetupStep(BaseModel):
    id: str
    label: str
    state: SetupStepState
    detail: str = ""
    action_label: str = ""
    href: str = ""


class SetupAccessUrl(BaseModel):
    kind: Literal["setup", "api", "mission_control", "video", "mavlink", "cloud"]
    label: str
    url: str
    source: Literal["local", "hotspot", "usb", "mdns", "cloud", "configured"]
    primary: bool = False


class RemoteAccessStatus(BaseModel):
    provider: Literal["none", "cloudflare"] = "none"
    enabled: bool = False
    configured: bool = False
    status: Literal["disabled", "configured", "running", "stopped", "error"] = "disabled"
    public_urls: list[str] = Field(default_factory=list)
    error: str = ""


class VideoAccess(BaseModel):
    state: str = "not_initialized"
    whep_url: str | None = None
    public_whep_url: str | None = None
    recording: bool = False


class MavlinkAccess(BaseModel):
    connected: bool = False
    port: str | None = None
    baud: int | None = None
    websocket_url: str | None = None
    public_websocket_url: str | None = None


class NetworkStatus(BaseModel):
    hostname: str = ""
    mdns_host: str = ""
    api_port: int = 8080
    hotspot_enabled: bool = False
    hotspot_ssid: str = ""
    local_ips: list[str] = Field(default_factory=list)


class ServiceState(BaseModel):
    """Light-weight service summary surfaced through the setup facade.

    The full per-service shape lives at /api/services. This model is the
    minimum the universal webapp and Mission Control's setup card need.
    """

    name: str
    state: str = "unknown"


class CloudChoiceStatus(BaseModel):
    """Cloud posture chosen during the onboarding wizard.

    Set by ``POST /api/v1/setup/cloud-choice`` and surfaced read-only on
    every ``SetupStatus`` response. Values mirror ``ServerConfig.mode``
    plus a small set of operator-facing diagnostics.
    """

    mode: Literal["cloud", "self_hosted", "local"] = "cloud"
    paired: bool = False
    pair_code_required: bool = True
    backend_url: str = ""  # display-only, never the API key
    backend_reachable: bool = False
    last_checked: str | None = None  # ISO 8601 IST


class ProfileSuggestion(BaseModel):
    """Result of the boot-time hardware fingerprint surfaced to the wizard.

    The wizard's profile step pre-selects ``detected`` and shows the per-
    signal map so the operator can sanity-check the auto-pick before
    confirming. ``confirmed`` flips true once the operator submits the
    profile step at least once for the active config value.
    """

    detected: Literal["drone", "ground_station", "unconfigured"] = "unconfigured"
    ground_role_hint: Literal["direct", "relay", "receiver"] = "direct"
    ground_score: int = 0
    air_score: int = 0
    mesh_capable: bool = False
    signals: dict[str, bool] = Field(default_factory=dict)
    confirmed: bool = False
    detected_at: str | None = None


class HardwareCheckItem(BaseModel):
    """One row in the hardware-check step's per-component readout."""

    id: str
    label: str
    required: bool = False
    state: Literal["ok", "missing", "warning", "checking", "unknown"] = "unknown"
    detail: str = ""
    fix_hint: str = ""


class HardwareCheckStatus(BaseModel):
    """Profile-aware hardware presence + readiness snapshot."""

    profile: str
    ground_role: str = ""
    items: list[HardwareCheckItem] = Field(default_factory=list)
    last_run: str = ""  # ISO 8601 IST


class SetupActionResult(BaseModel):
    ok: bool
    message: str
    data: dict[str, object] = Field(default_factory=dict)


class SetupStatus(BaseModel):
    version: str
    device_id: str
    device_name: str
    profile: str
    ground_role: str = ""
    setup_complete: bool
    setup_finalized: bool = False
    completion_percent: int
    next_action: str
    steps: list[SetupStep]
    access_urls: list[SetupAccessUrl]
    network: NetworkStatus
    mavlink: MavlinkAccess
    video: VideoAccess
    remote_access: RemoteAccessStatus
    services: list[ServiceState] = Field(default_factory=list)
    telemetry: dict[str, object] = Field(default_factory=dict)
    cloud_choice: CloudChoiceStatus = Field(default_factory=CloudChoiceStatus)
    profile_suggestion: ProfileSuggestion = Field(default_factory=ProfileSuggestion)
    hardware_check: HardwareCheckStatus | None = None
    skipped_steps: list[str] = Field(default_factory=list)
