"""MCP memory (World Model) tool handlers."""
from __future__ import annotations
from mcp.server.fastmcp import FastMCP
from ..shim import ShimError, get as shim_get, post as shim_post

def register(mcp: FastMCP) -> None:
    @mcp.tool(name="memory.observations.list")
    async def memory_obs_list(flight_id: str = "", limit: int = 50) -> dict:
        """List recent observations."""
        try:
            q = f"memory/observations?limit={limit}"
            if flight_id:
                q += f"&flight_id={flight_id}"
            return await shim_get(q)
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="memory.observations.get")
    async def memory_obs_get(obs_id: str) -> dict:
        """Get a single observation by ID."""
        try:
            return await shim_get(f"memory/observations/{obs_id}")
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="memory.observations.search")
    async def memory_obs_search(query: str, k: int = 10) -> dict:
        """Vector search observations by natural-language query."""
        try:
            return await shim_post("memory/search/by-text", {"query": query, "k": k})
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="memory.observations.tag")
    async def memory_obs_tag(obs_id: str, tag: str) -> dict:
        """Add a tag to an observation."""
        try:
            return await shim_post(f"memory/observations/{obs_id}/tags", {"tag": tag})
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="memory.entities.list")
    async def memory_entities_list(limit: int = 50) -> dict:
        """List merged entities from the World Model."""
        try:
            return await shim_get(f"memory/entities?limit={limit}")
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="memory.entities.get")
    async def memory_entities_get(entity_id: str) -> dict:
        """Get a single entity by ID."""
        try:
            return await shim_get(f"memory/entities/{entity_id}")
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="memory.entities.merge")
    async def memory_entities_merge(entity_id: str, merge_into: str) -> dict:
        """Merge entity_id into merge_into."""
        try:
            return await shim_post(f"memory/entities/{entity_id}/merge", {"merge_into": merge_into})
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="memory.entities.rename")
    async def memory_entities_rename(entity_id: str, name: str) -> dict:
        """Rename an entity."""
        try:
            return await shim_post(f"memory/entities/{entity_id}/rename", {"name": name})
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="memory.places.list")
    async def memory_places_list() -> dict:
        """List all saved places."""
        try:
            return await shim_get("memory/places")
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="memory.places.get")
    async def memory_places_get(place_id: str) -> dict:
        """Get a single place by ID."""
        try:
            return await shim_get(f"memory/places/{place_id}")
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="memory.place.add")
    async def memory_place_add(name: str, lat: float, lon: float, radius_m: float = 50.0) -> dict:
        """Save a new named place."""
        try:
            return await shim_post("memory/places", {"name": name, "lat": lat, "lon": lon, "radius_m": radius_m})
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="memory.flights.list")
    async def memory_flights_list(limit: int = 20) -> dict:
        """List recent flights in the World Model."""
        try:
            return await shim_get(f"memory/flights?limit={limit}")
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="memory.flights.get")
    async def memory_flights_get(flight_id: str) -> dict:
        """Get a single flight by ID."""
        try:
            return await shim_get(f"memory/flights/{flight_id}")
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="memory.frames.search_embedding")
    async def memory_frames_search(query: str, k: int = 5) -> dict:
        """Search frames by text query (embedding similarity)."""
        try:
            return await shim_post("memory/search/by-text", {"query": query, "k": k})
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="memory.diff")
    async def memory_diff(flight_id_a: str, flight_id_b: str) -> dict:
        """What changed between two flights (new/gone entities)."""
        try:
            return await shim_post("memory/diff", {"flight_id_a": flight_id_a, "flight_id_b": flight_id_b})
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="memory.snapshot")
    async def memory_snapshot(notes: str = "") -> dict:
        """Create a World Model snapshot (full DB dump)."""
        try:
            return await shim_post("memory/snapshots", {"notes": notes})
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="memory.sync_to_cloud")
    async def memory_sync_to_cloud(flight_id: str, exclude_fullres: bool = True) -> dict:
        """Sync a flight to the configured cloud endpoint."""
        try:
            return await shim_post(f"memory/sync?flight_id={flight_id}&exclude_fullres={exclude_fullres}", {})
        except ShimError as e:
            return {"status": "error", "message": str(e)}
