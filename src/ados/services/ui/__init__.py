"""Ground-station UI services (MSN-025, Wave A).

This package hosts the physical UI surfaces on the ground station
companion board: front-panel buttons (Wave A), OLED status display
(Wave B), and related input handling. Each module is independently
runnable via `python -m` for systemd supervision, matching the
pattern used by sibling services in `ados.services.ground_station`.

Wave A ships the button service and its event bus only. The OLED
service and screen renderers land in Wave B and consume the bus
contract defined in `events.py`.
"""
