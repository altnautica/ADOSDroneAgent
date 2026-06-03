"""Request + response models for the setup routes."""

from __future__ import annotations

from typing import Literal

from pydantic import BaseModel, Field

from ados.setup.models import (
    AdvancedApplyRequest,
    DisplayInstallRequest,
    NetworkApplyRequest,
    RegulatoryApplyRequest,
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
    regulatory: RegulatoryApplyRequest | None = None
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


