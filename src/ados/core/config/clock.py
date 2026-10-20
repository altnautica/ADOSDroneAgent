"""Host clock settings (the ``clock:`` block).

The native ``ados-clock`` service reads this block.
"""

from __future__ import annotations

from pydantic import BaseModel, Field


class ClockSimConfig(BaseModel):
    """Simulator time from the rendered video's SEI."""

    # Minimum latency, ns, from a rendered frame's simulator time to its
    # arrival on this host (encode, RTSP, demux, decode). One-way pairs cannot
    # measure it: when set it is added to the SEI envelope's offset; unset,
    # the offset is the envelope's and the published sigma carries a 30 ms
    # bound for it.
    latency_floor_ns: int | None = Field(default=None, ge=0, le=1_000_000_000)


class ClockConfig(BaseModel):
    sim: ClockSimConfig = Field(default_factory=ClockSimConfig)
