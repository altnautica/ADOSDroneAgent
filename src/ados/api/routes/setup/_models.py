"""Request + response models for the setup routes."""

from __future__ import annotations

from typing import Literal

from pydantic import BaseModel, Field

from ados.setup.models import (
    AdvancedApplyRequest,
    DisplayInstallRequest,
    NetworkApplyRequest,
    UiApplyRequest,
    WfbApplyRequest,
)


class CloudflareTokenRequest(BaseModel):
    token_or_script: str


class SelfHostedBackendRequest(BaseModel):
    url: str
    mqtt_broker: str = ""
    mqtt_port: int = 8883
    api_key: str = ""


class CloudChoiceRequest(BaseModel):
    mode: Literal["cloud", "self_hosted", "local"]
    self_hosted: SelfHostedBackendRequest | None = Field(default=None)


class ProfileChoiceRequest(BaseModel):
    profile: Literal["drone", "ground_station"]
    ground_role: Literal["direct", "relay", "receiver"] | None = Field(default=None)
    auto_restart: bool = Field(
        default=False,
        description=(
            "When true and the profile actually changed, dispatch a "
            "non-blocking ados-supervisor restart so the new profile's "
            "services come up without operator follow-up."
        ),
    )


class ApplyRequest(BaseModel):
    """Combined batch-apply payload sent by the settings sheet.

    Each field is optional. The route iterates the present sections in
    a fixed dependency order, calls each section's setter, and rolls
    back completed sections in reverse order on the first failure.
    """

    profile: ProfileChoiceRequest | None = None
    cloud: CloudChoiceRequest | None = None
    network: NetworkApplyRequest | None = None
    ui: UiApplyRequest | None = None
    wfb: WfbApplyRequest | None = None
    display: DisplayInstallRequest | None = None
    advanced: AdvancedApplyRequest | None = None


class ApplyResultSection(BaseModel):
    ok: bool
    message: str
    data: dict[str, object] = Field(default_factory=dict)


class ApplyResponse(BaseModel):
    """Per-section apply outcome.

    ``overall`` is True only when every present section returned ok.
    ``rolled_back`` lists sections that succeeded and were then
    reverted because a later section failed.
    """

    overall: bool
    sections: dict[str, ApplyResultSection] = Field(default_factory=dict)
    rolled_back: list[str] = Field(default_factory=list)


class CloudflareVerifyResponse(BaseModel):
    reachable: bool
    status_code: int | None = None
    latency_ms: int | None = None
    target_url: str | None = None
    error: str | None = None


class NavigationCapabilitiesResponse(BaseModel):
    """Active board's navigation capability summary."""

    vio_capable: bool
    csi_count: int
    usb_uvc_count: int
    rangefinder_ports: list[dict[str, str]] = Field(default_factory=list)


class NavigationCameraEntry(BaseModel):
    """One discovered camera annotated with its current + recommended role."""

    device: str
    name: str
    kind: str
    current_role: str = ""
    recommended_role: str = ""


class NavigationCamerasResponse(BaseModel):
    cameras: list[NavigationCameraEntry] = Field(default_factory=list)


class NavigationAssignCameraRequest(BaseModel):
    device_path: str = Field(..., min_length=1)
    role: Literal["nav", "secondary", "thermal", "inspection", "primary"]


class NavigationRangefinderDevice(BaseModel):
    path: str
    baud: int | None = None
    address: str | None = None


class NavigationRangefinderConfig(BaseModel):
    topology: Literal["companion", "fc"]
    driver: str
    device: NavigationRangefinderDevice


class NavigationConfigRequest(BaseModel):
    mode: Literal["off", "optical-flow", "vio", "both"]
    rangefinder: NavigationRangefinderConfig | None = None
    plugin_id: str | None = None
    # VIO direction. Only applies when mode is 'vio' or 'both'.
    # 'forward' fits indoor / corridor flight; 'downward' fits
    # over-ground flight (agriculture, survey, SAR, pipeline patrol);
    # 'auto' defers to the bound HAL camera role.
    vio_camera_orientation: Literal["forward", "downward", "auto"] | None = None
    # Flight controller firmware. Optical flow is supported on
    # ArduPilot, PX4, and iNav (7.0+). VIO is supported on ArduPilot
    # and PX4; the wizard rejects vio + inav. Betaflight is rejected
    # outright because the firmware has no position estimator.
    firmware: Literal["ardupilot", "px4", "inav"] | None = None


class NavigationPreflightResponse(BaseModel):
    frames_captured: int
    avg_quality: float
    mean_distance_m: float | None = None
    status: Literal["good", "low_quality", "no_frames", "no_camera"]
