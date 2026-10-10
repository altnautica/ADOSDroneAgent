"""Agent identity + profile configuration."""

from __future__ import annotations

from typing import Literal

from pydantic import BaseModel

# profile drives air vs ground-station behavior. "auto" triggers the
# boot-time hardware fingerprint in ados.bootstrap.profile_detect. A closed
# set, so the emitted schema carries the enum and the native config write
# refuses any other value instead of persisting one the services cannot load.
AgentProfile = Literal["auto", "drone", "ground_station", "workstation", "compute"]


class AgentConfig(BaseModel):
    device_id: str = ""
    name: str = "my-drone"
    tier: str = "auto"
    profile: AgentProfile = "auto"
    # Run with no Python services at all (native daemons only). Read by the
    # supervisor when it resolves which units the profile starts.
    headless: bool = False
