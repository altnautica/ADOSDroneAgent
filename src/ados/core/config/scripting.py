"""Scripting + REST API extensibility configuration."""

from __future__ import annotations

from pydantic import BaseModel

from ados.core.paths import SCRIPTS_DIR


class TextCommandsConfig(BaseModel):
    enabled: bool = True
    udp_port: int = 8889
    websocket_port: int = 8890


class ScriptsConfig(BaseModel):
    enabled: bool = True
    script_dir: str = str(SCRIPTS_DIR)
    max_concurrent: int = 3


class RestApiConfig(BaseModel):
    enabled: bool = True
    # IPv4 wildcard. See EndpointConfig.host comment — the actual
    # listener binds dual-stack via a separate helper that creates
    # both AF_INET and AF_INET6 sockets at startup.
    host: str = "0.0.0.0"
    port: int = 8080


class ScriptingConfig(BaseModel):
    text_commands: TextCommandsConfig = TextCommandsConfig()
    scripts: ScriptsConfig = ScriptsConfig()
    rest_api: RestApiConfig = RestApiConfig()
    # Optional explicit Mission Control URL surfaced through the setup
    # facade. When empty, the agent only advertises localhost:4000 to
    # operators who reached the setup webapp from localhost; everyone
    # else sees no link. Set this if Mission Control is reachable at a
    # known address (LAN IP, mDNS, tunnel, etc.).
    mission_control_url: str = ""
