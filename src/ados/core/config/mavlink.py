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
    # rewrite and lets it be set through the authenticated config surface.
    #
    # On, matching the router's own default. This declaration is not advisory:
    # persisting the config dumps every field, defaults included, so whatever
    # is written here becomes an explicit value in the node's file and an
    # explicit value beats the router's default. Declaring the opposite of the
    # router therefore does not merely disagree on paper -- the next config
    # write silently pins every node to the losing side, and the migration that
    # removes the stale recorded value is undone by the write that follows it.
    ws_proxy_enforce_auth: bool = True
    # When true, the raw byte-stream proxies (TCP 5760, UDP 14550) refuse an
    # unauthorized off-box peer instead of recording it and serving it anyway.
    #
    # Off, and the disagreement with the WebSocket flag above is the point.
    # The WebSocket enforces because a client can present either the
    # `X-ADOS-Key` header or an `ados-ws-ticket` subprotocol. The raw edges
    # have no credential channel at all -- no handshake, no headers -- so on a
    # paired node enforcement there refuses every off-box ground station with
    # nothing the client can do about it, and the published documentation tells
    # operators those ports are credential-free precisely so a desktop GCS can
    # attach. Declared here for the same reason as the flag above: the router
    # reads it, and a key this model does not declare is stripped from the file
    # on the next config write.
    raw_proxy_enforce_auth: bool = False
    # When true, a client the router could not authenticate is refused the
    # aux-radio uplink instead of being recorded and relayed anyway.
    #
    # Off. The uplink fallback is taken only on a node with no local flight
    # controller, i.e. a ground station relaying a drone that may be airborne,
    # so a refusal lands mid-flight on the operator's screen rather than at
    # install time. The relay's own declared forwarding is exempt from this
    # flag in every case, or remote piloting of a relayed drone stops working.
    aux_uplink_enforce_origin: bool = False
    # Whether the legacy `REQUEST_DATA_STREAM` group requests are sent alongside
    # the modern `SET_MESSAGE_INTERVAL` per-message requests on every stream
    # refresh. OFF by default: measured MAVLink ingest on ArduPilot was 66.5
    # frames/s against the 29 Hz the interval requests sum to, which is consistent
    # with the firmware honoring BOTH paths and streaming roughly twice the
    # telemetry that was asked for — on a radio link whose airtime is the binding
    # constraint for a fleet. The legacy path is the ONLY one iNav / Betaflight /
    # pre-4.1 ArduPilot honor, so it stays a flag rather than a deletion: if a
    # firmware's ingest collapses instead of halving, setting this true is the
    # rollback. Read by the native router, which owns the FC link.
    legacy_stream_request: bool = False

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
