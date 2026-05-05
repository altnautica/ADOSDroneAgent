"""Universal setup webapp assets — vanilla HTML / CSS / JS.

Mobile-first browser interface served by the agent's REST API at
``/api/v1/setup/*``. Pages share a single ES module dispatcher (app.js)
and a single stylesheet (style.css). Multiple agent backends in this
repository serve these assets from this canonical location, so the
operator UX stays identical regardless of which backend is running.

Pages:
  - index.html    — Status dashboard (FC, GPS, battery, system, services)
  - network.html  — WiFi AP settings, 4G modem status
  - video.html    — Camera preview, resolution / codec / bitrate
  - mavlink.html  — FC connection settings, proxy ports
  - system.html   — Resource gauges, agent info, reboot / reset, logs
  - setup.html    — First-boot setup wizard
  - ground.html   — Ground-station controls
  - remote.html   — Remote access (cloud relay, Cloudflare Tunnel)
  - advanced.html — Power-user controls
"""
