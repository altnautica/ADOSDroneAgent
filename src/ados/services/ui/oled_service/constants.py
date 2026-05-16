"""Module-level tunables shared across the OLED service modules.

Pin numbering, geometry, polling cadences, and brightness thresholds.
Split out so the service module can stay focused on lifecycle and
render orchestration without scrolling through display calibration
constants.
"""

from __future__ import annotations

# Button BCM pins, matching `button_service.py`.
B1, B2, B3, B4 = 5, 6, 13, 19

# Auto-cycle period and idle behavior (seconds).
AUTO_CYCLE_SECONDS = 5.0
IDLE_DIM_SECONDS = 60.0
INVERT_PERIOD_SECONDS = 600.0

# Brightness as luma.oled contrast values.
CONTRAST_ACTIVE = 80
CONTRAST_DIM = 40

# Display geometry.
WIDTH = 128
HEIGHT = 64

# Polling cadence for agent state. Status pages refresh slowly enough
# that a few seconds of staleness is invisible; sub-second polling burns
# CPU on a Pi-class SBC and crowds out the video pipeline's writer
# thread when the same node also serves the WFB-ng → mediamtx → WebRTC
# chain. 5 s gives the operator a fresh-enough status view at ~5x lower
# CPU cost. Pairing overlay still polls at PAIRING_POLL_SECONDS.
POLL_PERIOD_SECONDS = 5.0

# Idle floor for the LCD render loop. When the operator hasn't touched
# a button or the touchscreen for IDLE_LCD_FLOOR_SECONDS, the render
# tick stretches to at least 1 / IDLE_LCD_FLOOR_HZ regardless of what
# the active page's declared refresh_hz says. The dashboard's natural
# 5 Hz cadence is a waste of a full core on a benchtop SBC where the
# LCD is paint-the-same-clock-second over and over. Operator interaction
# (button press, touch gesture) resets _last_button_ts and the loop
# returns to the page's declared rate within one tick. Set the floor
# below the AUTO_CYCLE_SECONDS so the status carousel still advances
# while idle.
IDLE_LCD_FLOOR_SECONDS = 30.0
IDLE_LCD_FLOOR_HZ = 0.5

# Secondary poll cadence for pending-relay list while the Accept-window
# overlay is live. Faster than the main status poll so the operator sees
# incoming requests with minimal latency.
PAIRING_POLL_SECONDS = 0.5
