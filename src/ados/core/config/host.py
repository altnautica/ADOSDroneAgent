"""What kind of machine this agent is installed on, and how far it reaches.

The host kind itself lives in ``/etc/ados/host.conf`` (written by the installer,
read by the Rust supervisor, the control surface and the schema-driven UI), so it
is deliberately not a config field: it describes the machine, not a preference an
operator should be able to flip from a settings page and leave the running unit
set disagreeing with the file.

``share_on_lan`` is the one preference, and it only means anything on a personal
computer. An appliance serves the LAN the way it always has; a laptop or desktop
binds its control front and media server to loopback until its owner says
otherwise, because a ground station that answers on the office network the moment
it is installed is a surprise, not a feature.
"""

from pydantic import BaseModel, Field


class HostConfig(BaseModel):
    """Host-scoped exposure settings."""

    # Read once at startup by the control front (bind scope) and the media
    # server (WebRTC reach). Flipping it restarts both, so the change is live
    # without an install.
    share_on_lan: bool = Field(
        default=False,
        title="Share this ground station on the local network",
        description=(
            "Lets phones and tablets on your network reach this computer's "
            "ground station. Off: only this computer can."
        ),
    )
