"""MCP Tool handlers registered on the FastMCP server.

All tool groups are imported here and call register(mcp) to attach
their handlers to the server instance.
"""

from __future__ import annotations

from mcp.server.fastmcp import FastMCP


def register_all(mcp: FastMCP) -> None:
    """Register all tool groups on the given FastMCP instance."""
    from .flight import register as r_flight
    from .telemetry import register as r_telemetry
    from .params import register as r_params
    from .config_tools import register as r_config
    from .files import register as r_files
    from .services_tools import register as r_services
    from .video_tools import register as r_video
    from .vision_tools import register as r_vision
    from .memory_tools import register as r_memory
    from .mission_tools import register as r_mission
    from .ota_tools import register as r_ota
    from .system_tools import register as r_system
    from .ros_tools import register as r_ros
    from .agent_tools import register as r_agent

    from .assist import register as r_assist

    r_flight(mcp)
    r_telemetry(mcp)
    r_params(mcp)
    r_config(mcp)
    r_files(mcp)
    r_services(mcp)
    r_video(mcp)
    r_vision(mcp)
    r_memory(mcp)
    r_mission(mcp)
    r_ota(mcp)
    r_system(mcp)
    r_ros(mcp)
    r_agent(mcp)
    r_assist(mcp)
