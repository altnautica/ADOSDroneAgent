"""MAVLink endpoint + serial configuration."""

from __future__ import annotations

from typing import Literal

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
    # FC transport class the operator picked, surfaced as `fc_source` on the
    # status snapshot so the GCS/setup picker reflects the live choice:
    #   - `auto`   — discover + baud-probe any candidate serial port (the default)
    #   - `serial` — use the configured `serial_port` + `baud_rate`
    #   - `udp`/`tcp` — a network transport, with host:port carried in
    #     `serial_port` as `udp:host:port` / `tcp:host:port`
    # Default `auto` so an un-upgraded config behaves exactly as before.
    source: Literal["auto", "serial", "udp", "tcp"] = "auto"
    serial_port: str = ""
    baud_rate: int = 57600
    system_id: int = 1
    component_id: int = 191
    endpoints: list[EndpointConfig] = Field(default_factory=lambda: [
        EndpointConfig(type="websocket", port=8765, enabled=True),
    ])
    # When true, the raw MAVLink WebSocket proxy rejects an off-box connection
    # from a paired agent that presents no valid pairing key (the on-box and
    # unpaired paths stay open). The native router reads this same key from the
    # written config; declaring it here keeps it from being stripped on a config
    # rewrite and lets it be set through the authenticated config surface. Off by
    # default: the proxy logs an unauthorized connection but still admits it, so
    # enabling enforcement is an explicit, deliberate step.
    ws_proxy_enforce_auth: bool = False

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
