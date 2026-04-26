"""Version + capability negotiation endpoint.

The GCS calls /api/version on first connect to find out which agent
features it can rely on. Capabilities are stable string flags; new
features add a new flag and the GCS gates UI behavior on its presence.
This avoids the GCS hitting a 404 on a feature endpoint when paired
with an older agent, and lets a newer GCS keep working with an older
agent (gracefully hides the unavailable surface) without releasing
both repos in lockstep.
"""

from __future__ import annotations

from fastapi import APIRouter

from ados import __version__

router = APIRouter()

# Wire-protocol contract version. Bump when the request/response shape
# of any /api/* endpoint changes in a way the GCS must adapt to. The
# GCS reads this and picks compatible code paths.
API_VERSION = "1"

# Capability flags. Add a new flag whenever a new endpoint or behavior
# ships that the GCS may want to gate on. Never rename or remove a flag
# once shipped — the older GCS may rely on the absence to take a fallback
# code path. This list is the canonical surface contract between the
# agent and the GCS.
CAPABILITIES: list[str] = [
    # /api/status/full consolidated endpoint (fewer round-trips).
    "status.full",
    # /api/version endpoint (this one). Trivially true.
    "version.endpoint",
    # /api/services granular service control.
    "services.control",
    # /api/video/* live video pipeline state + transport switcher.
    "video.pipeline",
    # /api/wfb/* WFB-ng radio link control + telemetry.
    "wfb.link",
    # /api/scripts/* user-defined script runtime.
    "scripts.runtime",
    # /api/ota/* over-the-air updater.
    "ota.updater",
    # /api/pairing/* device-link mnemonic + token rotation.
    "pairing.mnemonic",
    # /api/peripherals/* legacy hardware scan + /v1 plugin registry.
    "peripherals.registry",
    # /api/suites/* mission suite activation.
    "suites.activation",
    # /api/fleet/* fleet roster surface.
    "fleet.roster",
    # /api/features/* HAL feature catalog.
    "features.catalog",
    # /api/ground-station/* full ground-agent profile surface.
    "ground_station.profile",
    # /api/ros/* opt-in ROS 2 Jazzy environment management.
    "ros.environment",
    # /api/signing/* MAVLink v2 signing key enrollment.
    "signing.mavlink",
    # WebRTC SDP signaling broker rejection surfaced via cloud status.
    "webrtc.signaling.last_error",
]


@router.get("/version")
async def get_version():
    """Wire-protocol version + capability flags.

    Stable shape. Add new keys at will; do not rename or remove existing
    keys without bumping `api_version`.
    """
    return {
        "api_version": API_VERSION,
        "agent_version": __version__,
        "capabilities": CAPABILITIES,
    }
