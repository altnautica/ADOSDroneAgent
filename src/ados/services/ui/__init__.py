"""Ground-station UI services.

This package hosts the physical UI surfaces on the ground station
companion board: front-panel buttons, OLED status display, and
related input handling. Each module is independently runnable via
`python -m` for systemd supervision, matching the pattern used by
sibling services in `ados.services.ground_station`.

The button service and its event bus are the input surface. The OLED
service and screen renderers consume the bus contract defined in
`events.py`.
"""
