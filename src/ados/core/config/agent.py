"""Agent identity + profile configuration."""

from __future__ import annotations

from pydantic import BaseModel, field_validator

# profile drives air vs ground-station behavior. "auto" triggers the
# boot-time hardware fingerprint in ados.bootstrap.profile_detect.
_ALLOWED_PROFILES = {"auto", "drone", "ground_station", "workstation", "compute"}


class AgentConfig(BaseModel):
    device_id: str = ""
    name: str = "my-drone"
    tier: str = "auto"
    profile: str = "auto"  # auto | drone | ground_station | workstation | compute

    @field_validator("profile")
    @classmethod
    def _validate_profile(cls, value: str) -> str:
        if value not in _ALLOWED_PROFILES:
            raise ValueError(
                f"agent.profile must be one of {sorted(_ALLOWED_PROFILES)}, got '{value}'"
            )
        return value
