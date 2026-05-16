"""Cloud server + remote-access configuration."""

from __future__ import annotations

from typing import Literal

from pydantic import BaseModel, Field

from ados.core.paths import CLOUDFLARE_TUNNEL_TOKEN_PATH


class CloudServerConfig(BaseModel):
    url: str = "https://convex-site.altnautica.com"
    mqtt_broker: str = "mqtt.altnautica.com"
    mqtt_port: int = 443


class SelfHostedServerConfig(BaseModel):
    url: str = ""
    mqtt_broker: str = ""
    mqtt_port: int = 8883
    api_key: str = ""


class ServerConfig(BaseModel):
    # Cloud posture set by the onboarding wizard's cloud-choice step.
    # `cloud` uses the Altnautica-managed Convex + MQTT backend;
    # `self_hosted` points at the operator's own deployment;
    # `local` disables the cloud relay entirely (Mission Control reaches
    # the agent directly over LAN / hotspot / USB tether).
    mode: Literal["cloud", "self_hosted", "local"] = "cloud"
    cloud: CloudServerConfig = CloudServerConfig()
    self_hosted: SelfHostedServerConfig = SelfHostedServerConfig()
    telemetry_rate: int = 2
    heartbeat_interval: int = 5
    mqtt_transport: str = "websockets"  # "tcp" or "websockets"
    mqtt_username: str = "ados"
    mqtt_password: str = ""  # Auto-filled from API key in cloud mode


class CloudflareTunnelConfig(BaseModel):
    enabled: bool = False
    token_path: str = str(CLOUDFLARE_TUNNEL_TOKEN_PATH)
    service_name: str = "cloudflared"
    setup_url: str = ""
    api_url: str = ""
    video_whep_url: str = ""
    mavlink_ws_url: str = ""


class RemoteAccessConfig(BaseModel):
    provider: Literal["none", "cloudflare"] = "none"
    public_urls: list[str] = Field(default_factory=list)
    cloudflare: CloudflareTunnelConfig = CloudflareTunnelConfig()
