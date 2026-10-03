"""REST API surface configuration.

Holds the bind address/port for the agent's main HTTP server (the
FastAPI surface the whole ``/api/pairing/*`` local-pairing path and the
setup webapp run on) plus the optional Mission Control URL the setup
facade advertises to operators.
"""

from __future__ import annotations

from pydantic import BaseModel, Field

from ._bounds import U16_MAX


class RestApiConfig(BaseModel):
    enabled: bool = True
    # IPv4 wildcard. See EndpointConfig.host comment — the actual
    # listener binds dual-stack via a separate helper that creates
    # both AF_INET and AF_INET6 sockets at startup.
    host: str = "0.0.0.0"
    port: int = Field(default=8080, ge=0, le=U16_MAX)


class ApiConfig(BaseModel):
    rest: RestApiConfig = RestApiConfig()
    # Optional explicit Mission Control URL surfaced through the setup
    # facade. When empty, the agent only advertises localhost:4000 to
    # operators who reached the setup webapp from localhost; everyone
    # else sees no link. Set this if Mission Control is reachable at a
    # known address (LAN IP, mDNS, tunnel, etc.).
    mission_control_url: str = ""
