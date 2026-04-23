"""MCP Prompts for the ADOS Drone Agent MCP server.

Each Prompt reads the relevant Resources server-side, interpolates
into a structured Markdown template, and returns a rendered brief.
No LLM runs on the drone. The intelligence is in the template.
"""

from __future__ import annotations

from mcp.server.fastmcp import FastMCP


def register_all(mcp: FastMCP) -> None:
    """Register all 6 Prompts on the MCP server."""
    _register_preflight(mcp)
    _register_postflight(mcp)
    _register_inspection(mcp)
    _register_site_familiarization(mcp)
    _register_config_audit(mcp)
    _register_troubleshoot(mcp)


def _register_preflight(mcp: FastMCP) -> None:
    @mcp.prompt(name="preflight_brief")
    async def preflight_brief() -> str:
        """Generate a pre-flight safety brief from live drone state.

        Reads telemetry, mission, and health. Returns structured Markdown.
        """
        from ..shim import ShimError, get as shim_get
        sections: list[str] = ["## Pre-flight Brief\n"]
        try:
            status = await shim_get("status/full")
            tel = status.get("telemetry", {})
            batt = tel.get("battery", {})
            gps = tel.get("gps", {})
            mode = tel.get("mode", "UNKNOWN")
            fc = status.get("fc_connected", False)
            sections.append(f"**FC Connected:** {'Yes' if fc else 'No'}")
            sections.append(f"**Mode:** {mode}")
            if batt:
                sections.append(
                    f"**Battery:** {batt.get('voltage_v', '?')}V, "
                    f"{batt.get('remaining_pct', '?')}% remaining"
                )
            if gps:
                sections.append(
                    f"**GPS:** {gps.get('num_sats', '?')} satellites, "
                    f"fix type {gps.get('fix_type', '?')}"
                )
        except ShimError as e:
            sections.append(f"**Status error:** {e}")
        try:
            mission = await shim_get("mission/current")
            wp_count = len(mission.get("waypoints", []))
            sections.append(f"**Mission:** {wp_count} waypoints loaded")
        except ShimError:
            sections.append("**Mission:** None loaded")
        try:
            svcs = await shim_get("services")
            svc_list = svcs if isinstance(svcs, list) else []
            unhealthy = [s["name"] for s in svc_list if s.get("state") not in ("running", "active")]
            sections.append(
                f"**Services degraded:** {', '.join(unhealthy)}"
                if unhealthy
                else "**Services:** All running"
            )
        except ShimError:
            pass
        sections.append("\n---\n*Generated from live drone state. Operator confirms go/no-go.*")
        return "\n\n".join(sections)


def _register_postflight(mcp: FastMCP) -> None:
    @mcp.prompt(name="postflight_debrief")
    async def postflight_debrief() -> str:
        """Generate a post-flight debrief from flight logs and telemetry."""
        from ..shim import ShimError, get as shim_get
        sections: list[str] = ["## Post-flight Debrief\n"]
        try:
            status = await shim_get("status/full")
            tel = status.get("telemetry", {})
            batt = tel.get("battery", {})
            sections.append(f"**Battery remaining:** {batt.get('remaining_pct', '?')}%")
        except ShimError as e:
            sections.append(f"**Status error:** {e}")
        try:
            logs = await shim_get("logs/recent?count=1")
            if logs and isinstance(logs, list) and logs:
                last = logs[0]
                sections.append(
                    f"**Last flight:** {last.get('duration_s', '?')}s, "
                    f"{last.get('distance_m', '?')}m"
                )
        except ShimError:
            sections.append("**Flight logs:** Not available")
        sections.append("\n---\n*Generated from agent data. Review logs for full analysis.*")
        return "\n\n".join(sections)


def _register_inspection(mcp: FastMCP) -> None:
    @mcp.prompt(name="inspection_review")
    async def inspection_review() -> str:
        """Summarize inspection findings from the World Model."""
        from ..shim import ShimError, get as shim_get
        sections: list[str] = ["## Inspection Review\n"]
        try:
            entities = await shim_get("memory/entities?limit=20")
            items = entities if isinstance(entities, list) else []
            if items:
                sections.append(f"**Entities detected:** {len(items)}")
                for item in items[:5]:
                    sections.append(
                        f"- {item.get('detect_class', '?')}: "
                        f"{item.get('observation_count', 0)} observations"
                    )
            else:
                sections.append("**No entities in World Model yet.**")
        except ShimError:
            sections.append("**World Model:** Not available")
        sections.append("\n---\n*Review from World Model. Enable ados-memory for full analysis.*")
        return "\n\n".join(sections)


def _register_site_familiarization(mcp: FastMCP) -> None:
    @mcp.prompt(name="site_familiarization")
    async def site_familiarization() -> str:
        """Generate a site brief from saved places and flight history."""
        from ..shim import ShimError, get as shim_get
        sections: list[str] = ["## Site Familiarization\n"]
        try:
            places = await shim_get("memory/places")
            place_list = places if isinstance(places, list) else []
            if place_list:
                sections.append(f"**Known places:** {len(place_list)}")
                for p in place_list[:5]:
                    sections.append(f"- {p.get('name', '?')} ({p.get('lat', '?')}, {p.get('lon', '?')})")
            else:
                sections.append("**Known places:** None saved yet")
        except ShimError:
            sections.append("**World Model:** Not available")
        sections.append("\n---\n*Site brief from World Model. Fly the site to build familiarity.*")
        return "\n\n".join(sections)


def _register_config_audit(mcp: FastMCP) -> None:
    @mcp.prompt(name="config_audit")
    async def config_audit() -> str:
        """Audit agent and FC configuration for drift from defaults."""
        from ..shim import ShimError, get as shim_get
        sections: list[str] = ["## Configuration Audit\n"]
        try:
            diff = await shim_get("params/diff")
            params = diff if isinstance(diff, list) else []
            if params:
                sections.append(f"**FC params differing from default:** {len(params)}")
                for p in params[:10]:
                    sections.append(
                        f"- {p.get('name', '?')}: "
                        f"current={p.get('value', '?')}, default={p.get('default', '?')}"
                    )
            else:
                sections.append("**FC params:** All at defaults")
        except ShimError as e:
            sections.append(f"**Params error:** {e}")
        sections.append("\n---\n*Audit from live agent + FC state.*")
        return "\n\n".join(sections)


def _register_troubleshoot(mcp: FastMCP) -> None:
    @mcp.prompt(name="troubleshoot_agent")
    async def troubleshoot_agent() -> str:
        """Generate a diagnostic snapshot for troubleshooting the agent."""
        from ..shim import ShimError, get as shim_get
        sections: list[str] = ["## Agent Diagnostic Snapshot\n"]
        try:
            full = await shim_get("status/full")
            svc_map = full.get("services", {})
            fc_ok = full.get("fc_connected", False)
            sections.append(f"**FC connected:** {fc_ok}")
            unhealthy = [
                k for k, v in svc_map.items()
                if isinstance(v, dict) and v.get("state") not in ("running", "active")
            ]
            sections.append(
                f"**Unhealthy services:** {', '.join(unhealthy)}"
                if unhealthy
                else "**Services:** All healthy"
            )
        except ShimError as e:
            sections.append(f"**Status error:** {e}")
        sections.append("\n---\n*Snapshot from live agent state.*")
        return "\n\n".join(sections)
