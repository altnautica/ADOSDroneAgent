"""Cloud server + remote-access configuration."""

from __future__ import annotations

from typing import Literal

from pydantic import BaseModel, Field

from ados.core.paths import CLOUDFLARE_TUNNEL_TOKEN_PATH

from ._bounds import U16_MAX


class CloudServerConfig(BaseModel):
    url: str = "https://convex-site.altnautica.com"
    mqtt_broker: str = "mqtt.altnautica.com"
    mqtt_port: int = Field(default=443, ge=0, le=U16_MAX)


class SelfHostedServerConfig(BaseModel):
    url: str = ""
    mqtt_broker: str = ""
    mqtt_port: int = Field(default=8883, ge=0, le=U16_MAX)
    api_key: str = ""


class ServerConfig(BaseModel):
    # Cloud posture set by the onboarding wizard's cloud-choice step.
    # `local` disables the cloud relay entirely (Mission Control reaches
    # the agent directly over LAN / hotspot / USB tether) and is the
    # install-time default — fresh boards are fully usable without any
    # cloud uplink. `cloud` uses the Altnautica-managed Convex + MQTT
    # backend; `self_hosted` points at the operator's own deployment.
    mode: Literal["cloud", "self_hosted", "local"] = "local"
    cloud: CloudServerConfig = CloudServerConfig()
    self_hosted: SelfHostedServerConfig = SelfHostedServerConfig()
    # The relay authenticates as `ados-<device_id>` with the pairing key over
    # TLS; the transport is the only MQTT setting it reads from this block.
    mqtt_transport: str = "websockets"  # "tcp" or "websockets"
    # Allow an operator-triggered export of node logs to the cloud. There is no
    # continuous upload; this is the whole gate on any export.
    cloud_logs_enabled: bool = False


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
