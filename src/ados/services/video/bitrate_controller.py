"""The bitrate / FEC ladder table.

The ladder the adaptive controller steps through. The controller itself is
native (`crates/ados-radio`, `BitrateController`), which owns the sampling loop,
the hysteresis and the actuation; what stays here is the rung table two REST
write paths read to resolve a pinned `tier_idx` back to its bitrate and FEC
pair.

The Python controller that used to live in this file was never instantiated
anywhere, so its snapshot writer never ran and the
`/run/ados/bitrate-controller.json` it claimed to persist never existed --
which is what left the `adaptive` block of `GET /api/video/config` permanently
empty. The live state is published in `wfb-stats.json` by the native
controller, and both config routes read it there now.
"""

from __future__ import annotations

from dataclasses import dataclass


@dataclass(frozen=True)
class BitrateTier:
    """One rung on the bitrate / FEC ladder.

    bitrate_kbps drives the video encoder. fec_k / fec_n drive the
    wfb_tx Reed-Solomon configuration. Each tier should be playable
    independently — no implicit pairing assumptions beyond what's
    written here.
    """

    name: str
    bitrate_kbps: int
    fec_k: int
    fec_n: int


# Index 0 is the high-quality default; the controller climbs back to
# this tier whenever the link permits. Index -1 is the rescue tier
# the controller falls to on a very degraded link — bitrate is still
# enough for a recognizable framerate, FEC ratio is 8/4 = 200% so
# every block survives 2 packet losses.
DEFAULT_TIERS: tuple[BitrateTier, ...] = (
    BitrateTier("high", 4000, 8, 12),
    BitrateTier("medium", 3000, 8, 14),
    BitrateTier("low", 2000, 8, 16),
    BitrateTier("rescue", 1200, 4, 12),
)
