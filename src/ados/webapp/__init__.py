"""Config webapp — vanilla HTML/JS/CSS served by the agent's REST API.

Ported from ADOS Agent Lite. Provides a browser-based configuration
interface accessible via the drone's WiFi AP. Mobile-first, dark theme,
no web fonts or external dependencies.

Pages:
  - index.html    — Status dashboard (FC, GPS, battery, system, services)
  - network.html  — WiFi AP settings, 4G modem status
  - video.html    — Camera preview, resolution/codec/bitrate settings
  - mavlink.html  — FC connection settings, proxy ports
  - system.html   — Resource gauges, agent info, reboot/reset, logs
  - setup.html    — First-boot setup wizard (6 steps)
"""
