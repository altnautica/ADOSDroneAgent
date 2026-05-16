"""MAVLink endpoint + serial configuration."""

from __future__ import annotations

from pydantic import BaseModel, Field, model_validator


class EndpointConfig(BaseModel):
    type: str = "websocket"
    # IPv4 wildcard. The agent's network entry points (REST + MAVLink WS)
    # bind explicit dual-stack sockets at startup via a helper that
    # creates one AF_INET listener AND one AF_INET6 listener, so the
    # `host` here is interpreted as the IPv4 bind address. The IPv6
    # leg is added implicitly by the dual-bind helper. Binding to "::"
    # alone is unreliable across kernels (uvicorn's IPv6-only fallback
    # left IPv4 unreachable on the bench Pi).
    host: str = "0.0.0.0"
    port: int = 8765
    enabled: bool = True


class MavlinkConfig(BaseModel):
    serial_port: str = ""
    baud_rate: int = 57600
    system_id: int = 1
    component_id: int = 191
    endpoints: list[EndpointConfig] = Field(default_factory=lambda: [
        EndpointConfig(type="websocket", port=8765, enabled=True),
    ])

    @model_validator(mode="before")
    @classmethod
    def _drop_legacy_signing(cls, values):
        """Strip legacy mavlink.signing block from old config files.

        The prior SigningConfig scaffolding never held a live key and is
        now removed. MAVLink message signing is owned by the GCS browser;
        the agent does not persist key material.
        """
        if isinstance(values, dict) and "signing" in values:
            values.pop("signing", None)
        return values
