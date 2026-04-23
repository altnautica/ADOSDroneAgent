"""MCP memory_tools tool handlers.

Safety class: read (default for this group; some tools may vary).
All handlers are stubs returning not_implemented status.
Full implementation ships in Phase 2.
"""

from __future__ import annotations

from mcp.server.fastmcp import FastMCP


def register(mcp: FastMCP) -> None:
    """Register memory_tools tools on the MCP server."""

    @mcp.tool(name="memory.observations.list")
    def memory_observations_list(**kwargs: object) -> dict:
        """Phase 1 stub for memory.observations.list."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="memory.observations.get")
    def memory_observations_get(**kwargs: object) -> dict:
        """Phase 1 stub for memory.observations.get."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="memory.observations.search")
    def memory_observations_search(**kwargs: object) -> dict:
        """Phase 1 stub for memory.observations.search."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="memory.observations.tag")
    def memory_observations_tag(**kwargs: object) -> dict:
        """Phase 1 stub for memory.observations.tag."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="memory.entities.list")
    def memory_entities_list(**kwargs: object) -> dict:
        """Phase 1 stub for memory.entities.list."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="memory.entities.get")
    def memory_entities_get(**kwargs: object) -> dict:
        """Phase 1 stub for memory.entities.get."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="memory.entities.merge")
    def memory_entities_merge(**kwargs: object) -> dict:
        """Phase 1 stub for memory.entities.merge."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="memory.entities.rename")
    def memory_entities_rename(**kwargs: object) -> dict:
        """Phase 1 stub for memory.entities.rename."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="memory.places.list")
    def memory_places_list(**kwargs: object) -> dict:
        """Phase 1 stub for memory.places.list."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="memory.places.get")
    def memory_places_get(**kwargs: object) -> dict:
        """Phase 1 stub for memory.places.get."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="memory.place.add")
    def memory_place_add(**kwargs: object) -> dict:
        """Phase 1 stub for memory.place.add."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="memory.flights.list")
    def memory_flights_list(**kwargs: object) -> dict:
        """Phase 1 stub for memory.flights.list."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="memory.flights.get")
    def memory_flights_get(**kwargs: object) -> dict:
        """Phase 1 stub for memory.flights.get."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="memory.frames.search_embedding")
    def memory_frames_search_embedding(**kwargs: object) -> dict:
        """Phase 1 stub for memory.frames.search_embedding."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="memory.diff")
    def memory_diff(**kwargs: object) -> dict:
        """Phase 1 stub for memory.diff."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="memory.snapshot")
    def memory_snapshot(**kwargs: object) -> dict:
        """Phase 1 stub for memory.snapshot."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="memory.sync_to_cloud")
    def memory_sync_to_cloud(**kwargs: object) -> dict:
        """Phase 1 stub for memory.sync_to_cloud."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}
