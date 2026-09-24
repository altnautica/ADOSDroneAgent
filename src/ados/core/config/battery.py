"""Battery health engine thresholds (the ``battery:`` block).

Every field is a bounded integer because the GCS config primitives carry no
float input: voltages are millivolts, the temperature-rise rate is in tenths
of a degree Celsius per second (``temp_spike_dc_per_s = 50`` means 5.0 °C/s).
The native battery engine reads the same block and falls back to the default
field-wise when a stored value is out of bounds; this model rejects such a
write at the config surface so an operator never stores one.
"""

from __future__ import annotations

from pydantic import BaseModel, Field, model_validator


class BatteryConfig(BaseModel):
    """Per-pack cell, sag, temperature and time-to-reserve thresholds.

    ``critical_cell_mv`` must stay below ``low_cell_mv``, otherwise the
    warning band between the two collapses and ``cell_low`` can never fire
    before ``cell_critical``.
    """

    enabled: bool = True
    low_cell_mv: int = Field(default=3500, ge=2500, le=4200)
    critical_cell_mv: int = Field(default=3300, ge=2500, le=4000)
    cell_divergence_mv: int = Field(default=50, ge=10, le=500)
    voltage_drop_mv_per_s: int = Field(default=500, ge=100, le=5000)
    temp_spike_dc_per_s: int = Field(default=50, ge=5, le=200)
    predictive_window_s: int = Field(default=30, ge=5, le=300)
    reserve_percent: int = Field(default=25, ge=5, le=50)

    @model_validator(mode="after")
    def _critical_below_low(self) -> BatteryConfig:
        if self.critical_cell_mv >= self.low_cell_mv:
            raise ValueError(
                "battery.critical_cell_mv must be less than battery.low_cell_mv"
            )
        return self
