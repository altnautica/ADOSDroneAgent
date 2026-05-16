"""Stats snapshot owned by the air-side pipeline thread.

Mutable in place so the GStreamer streaming thread can update the
fields without allocating; ``to_dict`` yields the immutable snapshot
the REST surface and the heartbeat enricher consume.
"""

from __future__ import annotations

from typing import Any


class AirPipelineStats:
    """Mutable snapshot the AirPipeline thread updates in place.

    A plain dataclass would be just as fine; the class form makes the
    REST surface's ``response_model`` mapping a little cleaner and
    keeps every field bounded to a known type.
    """

    __slots__ = (
        "camera_source",
        "encoder_name",
        "encoder_hw_accel",
        "pipeline_state",
        "started_at",
        "last_state_change_at",
        "encoder_fps",
        "encoded_kbps",
        "sei_injected_count",
        "udp_bytes_out",
        "last_buffer_at",
        "restart_count",
        "tx_silent_kicks",
        "bus_errors",
        "cloud_branch_open",
    )

    def __init__(self) -> None:
        self.camera_source: str = ""
        self.encoder_name: str = ""
        self.encoder_hw_accel: bool = False
        self.pipeline_state: str = "idle"
        self.started_at: float | None = None
        self.last_state_change_at: float | None = None
        self.encoder_fps: float = 0.0
        self.encoded_kbps: float = 0.0
        self.sei_injected_count: int = 0
        self.udp_bytes_out: int = 0
        self.last_buffer_at: float | None = None
        self.restart_count: int = 0
        self.tx_silent_kicks: int = 0
        self.bus_errors: int = 0
        self.cloud_branch_open: bool = False

    def to_dict(self) -> dict[str, Any]:
        return {
            "camera_source": self.camera_source,
            "encoder_name": self.encoder_name,
            "encoder_hw_accel": self.encoder_hw_accel,
            "pipeline_state": self.pipeline_state,
            "started_at": self.started_at,
            "last_state_change_at": self.last_state_change_at,
            "encoder_fps": round(self.encoder_fps, 2),
            "encoded_kbps": round(self.encoded_kbps, 1),
            "sei_injected_count": int(self.sei_injected_count),
            "udp_bytes_out": int(self.udp_bytes_out),
            "last_buffer_at": self.last_buffer_at,
            "restart_count": int(self.restart_count),
            "tx_silent_kicks": int(self.tx_silent_kicks),
            "bus_errors": int(self.bus_errors),
            "cloud_branch_open": bool(self.cloud_branch_open),
        }
