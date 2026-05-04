"""Pydantic models for the universal setup surface."""

from __future__ import annotations

from typing import Literal

from pydantic import BaseModel, Field

SetupStepState = Literal["complete", "needs_action", "optional", "blocked"]


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


class SetupActionResult(BaseModel):
    ok: bool
    message: str
    data: dict[str, object] = Field(default_factory=dict)


class SetupStatus(BaseModel):
    version: str
    device_id: str
    device_name: str
    profile: str
    setup_complete: bool
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
