"""Storage housekeeping and hardware-gate configuration.

Both blocks are read only by native code: ``storage.janitor`` by the
supervisor's disk janitor, ``hardware`` by the installer. They are modelled
here so their defaults are stated once, in the schema every surface reads.
"""

from __future__ import annotations

from pydantic import BaseModel, Field

from ._bounds import I64_MAX


class JanitorCapsConfig(BaseModel):
    """Per-category disk caps, in MiB."""

    log_store_mb: int = Field(default=1200, ge=0, le=I64_MAX)
    quarantined_stores_mb: int = Field(default=400, ge=0, le=I64_MAX)
    recordings_mb: int = Field(default=2400, ge=0, le=I64_MAX)
    plugin_logs_mb: int = Field(default=256, ge=0, le=I64_MAX)
    audit_log_mb: int = Field(default=64, ge=0, le=I64_MAX)
    journal_mb: int = Field(default=400, ge=0, le=I64_MAX)
    apt_mb: int = Field(default=400, ge=0, le=I64_MAX)


class JanitorConfig(BaseModel):
    """The supervisor's disk janitor: trims logs, recordings and the journal
    against a total budget and the free-space thresholds."""

    enabled: bool = True
    # Seconds between passes (floor 60).
    interval_s: int = Field(default=3600, ge=0, le=I64_MAX)
    pressure_free_pct: float = 20.0
    critical_free_pct: float = 10.0
    plugin_log_max_mb: int = Field(default=32, ge=0, le=I64_MAX)
    plugin_log_keep_mb: int = Field(default=8, ge=0, le=I64_MAX)
    audit_max_mb: int = Field(default=16, ge=0, le=I64_MAX)
    audit_keep_mb: int = Field(default=4, ge=0, le=I64_MAX)
    recording_retention_days: int = Field(default=90, ge=0, le=I64_MAX)
    recording_pressure_retention_days: int = Field(default=14, ge=0, le=I64_MAX)
    recording_keep_newest: int = Field(default=3, ge=0, le=I64_MAX)
    journal_pressure_mb: int = Field(default=128, ge=0, le=I64_MAX)
    journal_floor_mb: int = Field(default=64, ge=0, le=I64_MAX)
    quarantine_keep_newest: int = Field(default=1, ge=0, le=I64_MAX)
    budget_mb: int = Field(default=5120, ge=0, le=I64_MAX)
    caps: JanitorCapsConfig = JanitorCapsConfig()


class StorageConfig(BaseModel):
    janitor: JanitorConfig = JanitorConfig()


class HardwareConfig(BaseModel):
    # Un-gate the native GPIO output service (the ground station's beeper and
    # status outputs). The installer reads it.
    gpio_out_enabled: bool = True
