"""Command authority (``control:``) and the ADOS pilot flight model (``pilot:``).

``control.mode`` decides where operator sticks go: ``ados`` (the on-board
pilot flies them, the flight controller runs GUIDED), ``fc_manual`` (ground
control MANUAL_CONTROL goes straight to the flight controller) or ``elrs``
(the ground station's ELRS link drives the flight controller's RC input).
The pilot daemon persists a mode change made from an operator surface back
into this block.

``pilot:`` holds the pilot's stick profile, obstacle handling, limits and
link-loss behaviour. Live changes of ``profile`` and ``avoid`` from an
operator surface are persisted here as well.
"""

from __future__ import annotations

from typing import Literal

from pydantic import BaseModel, Field


class ControlConfig(BaseModel):
    """Where operator sticks go, and the RC switch slot labelled ADOS."""

    mode: Literal["ados", "fc_manual", "elrs"] = "ados"
    # The flight-mode switch slot (1-6) the setup wizard assigned to GUIDED and
    # labelled "ADOS"; 0 means none assigned.
    rc_ados_position: int = Field(default=0, ge=0, le=6)


class PilotConfig(BaseModel):
    """Stick profile, height reference, obstacle handling and limits."""

    profile: Literal["cine", "normal", "sport"] = "normal"
    avoid: Literal["brake", "bypass", "off"] = "brake"
    height_ref: Literal["agl", "ekf"] = "agl"
    obstacle_margin_m: float = Field(default=1.5, ge=0.5, le=5.0)
    max_alt_agl_m: float = Field(default=120.0, ge=5.0, le=500.0)
    max_dist_m: float = Field(default=500.0, ge=20.0, le=10000.0)
    min_agl_fw_m: float = Field(default=30.0, ge=10.0, le=300.0)
    link_loss_s: int = Field(default=10, ge=3, le=120)
    link_loss_action: Literal["rth", "hold", "land"] = "rth"
    # Horizontal speed (m/s) and acceleration (m/s^2) caps the pilot applies to
    # every setpoint it flies while the EKF's horizontal position is not GNSS.
    gnss_denied_max_speed_mps: float = Field(default=2.0, ge=0.5, le=15.0)
    gnss_denied_max_accel_mps2: float = Field(default=1.5, ge=0.5, le=6.0)
