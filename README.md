# ADOS Drone Agent

**Open-source onboard agent for software-defined drones. 50km data link. HD video. Full remote control.**

![License: GPL-3.0](https://img.shields.io/badge/License-GPL--3.0-green.svg) ![Hybrid: Rust + Python](https://img.shields.io/badge/Hybrid-Rust%20%2B%20Python-blue.svg) ![Status: Alpha](https://img.shields.io/badge/Status-Alpha-orange.svg) [![Discord](https://img.shields.io/badge/Discord-Join-5865F2.svg)](https://discord.gg/uxbvuD4d5q)

ADOS Drone Agent is the onboard intelligence layer for software-defined drones. It runs on your companion computer, proxies MAVLink from the flight controller to WebSocket and TCP, handles the 50km data link, streams HD video, and gives you full remote control from ADOS Mission Control or any HTTP client.

> **Part of the ADOS ecosystem.** Pairs with [ADOS Mission Control](https://github.com/altnautica/ADOSMissionControl) (the browser GCS) for AI PID tuning, mission planning, 3D simulation, live ADS-B, and gamepad flight control at 50Hz. The agent runs on the drone; Mission Control runs in your browser. Extend either side with [ADOS Extensions](https://github.com/altnautica/ADOSExtensions), the first-party plugin repo.

<p align="center">
  <strong><a href="https://github.com/altnautica/ADOSMissionControl">ADOS Mission Control</a></strong> |
  <strong><a href="https://github.com/altnautica/ADOSExtensions">ADOS Extensions</a></strong> |
  <strong><a href="https://docs.altnautica.com">Docs</a></strong> |
  <strong><a href="https://altnautica.com">Website</a></strong> |
  <strong><a href="https://discord.gg/uxbvuD4d5q">Discord</a></strong> |
  <strong><a href="mailto:team@altnautica.com">Email</a></strong> |
  <strong><a href="https://github.com/altnautica/ADOSDroneAgent/issues">Issues</a></strong>
</p>

---

<table>
  <tr>
    <td width="50%">
      <img src="docs/screenshots/overview.png" alt="ADOS Drone Agent overview showing services, system resources, and logs" height="220" width="100%"><br>
      <sub>Overview tab, showing running services, system resources, and live logs in Mission Control</sub>
    </td>
    <td width="50%">
      <img src="docs/screenshots/scripts.png" alt="Python script editor with syntax highlighting for drone automation" height="220" width="100%"><br>
      <sub>Script editor with syntax highlighting for Python drone automation</sub>
    </td>
  </tr>
  <tr>
    <td width="50%">
      <img src="docs/screenshots/fleet-network.png" alt="Fleet network enrollment, MQTT gateway, mesh radio peers" height="220" width="100%"><br>
      <sub>Fleet network enrollment, MQTT gateway status, and mesh radio peers</sub>
    </td>
    <td width="50%">
      <img src="docs/screenshots/peripherals.png" alt="Connected peripherals with live sensor readings" height="220" width="100%"><br>
      <sub>Connected peripherals with live sensor readings; drivers extend through the plugin system</sub>
    </td>
  </tr>
</table>

---

## Architecture at a glance

ADOS Drone Agent is a hybrid. The long-running and safety-critical services are native **Rust**: the process supervisor that drives systemd, the MAVLink router, the video pipeline, the cloud relay, the WFB-ng radio control, on-device vision, and the ground-station receive and uplink paths. **Python** runs the layers that change fast or lean on the ML and web ecosystems: the FastAPI REST server and setup webapp, AI and vision inference, hardware detection, the scripting engine and SDK, and the plugin runtime. A few mature **C** programs are supervised rather than reimplemented (the RTL8812EU driver, `wfb_tx` / `wfb_rx`, `ffmpeg`, and `mediamtx`).

systemd is the process manager. The Rust supervisor orchestrates the units, it does not replace systemd. On a device the agent installs as a set of systemd services plus a Python virtualenv, both placed by a prebuilt Rust installer.

---

## Quick Start

Deploy to a companion computer (Raspberry Pi, Radxa, Jetson, and similar ARM64 Linux boards):

```bash
curl -sSL https://raw.githubusercontent.com/altnautica/ADOSDroneAgent/main/scripts/install.sh | sudo bash
```

Root is required. The one-line bootstrap fetches and verifies a prebuilt Rust installer, which provisions a Python virtualenv, places the native Rust service binaries, configures the systemd units, and starts the local setup webapp on the device.

After install, SSH into the node and run:

```bash
ados
```

The terminal status page shows the local setup URL, LAN or hotspot URLs, MAVLink
state, video state, services, and remote access state.

### System Requirements

| Requirement | Minimum | Recommended |
|-------------|---------|-------------|
| OS | Linux with systemd (ARM64 or x86_64) | Raspberry Pi OS, Debian, Ubuntu, Armbian |
| RAM | 1 GB | 2 GB+ |
| Storage | 500 MB | 2 GB+ |
| Python | 3.11+ | 3.12 |
| FC connection | Serial (UART or USB) | UART at 921600 baud |

Also runs on macOS for local development (the installer points you to `cargo` there). Boards with less than 1 GB of RAM are not supported.

## What It Does

**MAVLink proxy.** Reads the FC serial port and routes MAVLink to WebSocket, TCP, and UDP simultaneously. Multiple ground stations can connect at once. Auto-reconnect on FC disconnect.

**50km data link.** When paired with ADOS Mission Control, the agent publishes telemetry via MQTT over a Cloudflare Tunnel at 2Hz+. No port forwarding needed. Works from anywhere with a cellular connection.

**HD video streaming.** The video pipeline supports RTSP, WebRTC/WHEP, and WFB-ng radio paths. Mission Control can play live feeds in the browser over local or relayed connections.

**Full remote control.** The GCS can send arm/disarm, mode changes, guided flight commands, and mission uploads through the cloud relay. The agent polls and executes them. All from a browser, over any network.

**Local setup webapp.** The agent self-hosts a mobile-friendly setup app at
`:8080`. Use it for first-run identity, MAVLink, video, network, remote access,
ground-station setup, logs, and advanced recovery.

**REST API.** FastAPI server at `:8080` with domain route modules. The
`/api/v1/setup/status` facade feeds the webapp, CLI, Mission Control, Android
handoff, and support tooling. Full OpenAPI docs are at `/docs`.

**MAVLink signing.** The agent is a transparent pipe for MAVLink v2 signed frames. `/api/mavlink/signing/*` exposes capability detection and one-shot FC enrollment via `SETUP_SIGNING`. Keys live in the GCS browser; the agent holds no key material. See [docs](https://docs.altnautica.com/drone-agent/mavlink-signing).

**Terminal status page.** Run `ados` over SSH for a read-only full-screen status
page. It points you to the setup webapp and shows MAVLink, video, network,
remote access, services, and telemetry at a glance.

**Hardware auto-detection.** Detects board tier on boot (RPi Zero 2W through CM5 / Jetson) and enables services based on available resources.

**Ground station mode.** The same agent codebase runs on a ground SBC. A hardware fingerprint at boot picks the `ground_station` profile (OLED on I2C plus four GPIO buttons plus an RTL8812EU adapter, no flight controller) versus the drone profile. Within the ground-station profile the node runs in one of three deployment roles described below.

**Distributed receive and local mesh.** Two or three Ground Agents can be deployed together for obstructed flight areas. A `receiver` node hubs the deployment; one or more `relay` nodes forward WFB-ng fragments to the receiver over a self-healing batman-adv mesh on a second USB WiFi dongle. The receiver runs WFB-ng's native FEC combine across the merged stream. Pairing is field-only via the OLED in 60 seconds, no laptop required. See `docs.altnautica.com/ground-agent/mesh-overview` for the full picture.

---

## Hardware Support

The agent auto-detects the board on boot and scales features to available resources. Board profiles ship in `src/ados/hal/boards/`.

| Class | Example boards | Capabilities |
|-------|----------------|--------------|
| Entry | Raspberry Pi 3, Radxa CM3 (RK3566) | MAVLink proxy, cloud relay, telemetry |
| Standard | Raspberry Pi 4 / 5, CM4, Orange Pi 5 | + HD video pipeline, WFB-ng radio, scripting |
| Accelerated | CM5, Rock 5C, Cubie A7Z, RK3576 / RK3588S2, Jetson Nano / Orin Nano | + on-device vision, NPU-backed inference, plugin sandbox |

Any ARM64 or x86_64 Linux board with a serial port and systemd should work through the `generic-arm64` profile. The same codebase also runs the ground-station profile (no flight controller, an OLED or SPI LCD with buttons and an RTL8812EU adapter); a hardware fingerprint at boot picks the profile.

**Mesh role hardware.** A single ground node (`direct` role) needs one RTL8812EU USB WiFi adapter for WFB-ng. Relay and receiver nodes add a second USB WiFi dongle that carries batman-adv mesh traffic between nodes. Any adapter with a Linux driver that supports 802.11s or IBSS mode works for the mesh carrier.

---

## CLI Reference

The public everyday CLI is intentionally small. Run `ados --help` for the
current command list.

| Command | Description |
|---------|-------------|
| `ados` | Open the read-only terminal status page, or plain status when not attached to a TTY |
| `ados status` | Print setup, MAVLink, video, network, remote access, and service status |
| `ados status --json` | Print the full setup facade payload for scripts |
| `ados update` | Check for and install an agent update |
| `ados update --check-only` | Check without installing |
| `ados uninstall` | Remove the agent from the system |
| `ados uninstall --purge --yes` | Remove services, package files, and config without prompts |

Use the local setup webapp or Mission Control Hardware tab for configuration,
ground-station pairing, video, network, remote access, logs, and advanced
actions.

---

## REST API

FastAPI server at `:8080`. Full OpenAPI docs at `/docs`. Domain route modules cover the agent, ground-station profile, plugins, and optional subsystems.

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/api/v1/setup/status` | GET | Universal setup, access URL, MAVLink, video, network, and remote-access status |
| `/api/v1/setup/remote-access/cloudflare` | POST | Install a Cloudflare Tunnel token or install command |
| `/api/status` | GET | Agent status, uptime, FC state |
| `/api/telemetry` | GET | Attitude, GPS, battery snapshot |
| `/api/params` | GET | Read cached FC parameters |
| `/api/command` | POST | Send MAVLink command to FC |
| `/api/commands` | GET | List supported command names |
| `/api/config` | GET / PUT | Read or update agent config |
| `/api/logs` | GET | Recent log entries |
| `/api/services` | GET | Running services and status |
| `/api/video` | GET / POST | Video pipeline status and control |
| `/api/scripts` | GET / POST | List and execute automation scripts |
| `/api/plugins` | GET / POST / DELETE | Plugin install, list, enable, disable, remove, info |
| `/api/fleet/*` | GET | Fleet enrollment and peer status |
| `/api/peripherals` | GET | Connected sensors and hardware |
| `/api/pairing/*` | GET / POST | GCS pairing management |
| `/api/system` | GET / POST | System info, reboot, shutdown |
| `/api/ota` | GET / POST | Update check, upgrade, rollback |
| `/api/v1/ground-station/*` | GET / PUT / POST / DELETE | Ground-station profile only. Role, mesh, pairing, WFB-ng relay/receiver, uplinks, physical UI |

```bash
# Get current telemetry
curl http://localhost:8080/api/telemetry

# Arm the drone
curl -X POST http://localhost:8080/api/command \
  -H "Content-Type: application/json" \
  -d '{"command": "arm"}'
```

---

## Cloud Connectivity

The agent connects to ADOS Mission Control over a three-layer relay.

**Convex HTTP (baseline).** Every 5 seconds, the agent POSTs full status to the cloud. The GCS reads via reactive Convex queries. Commands go the reverse direction. Zero extra infra required.

**MQTT telemetry (real-time).** When `server.mode` is `cloud` or `self_hosted`, the agent publishes to `ados/{deviceId}/status` and `ados/{deviceId}/telemetry` via Mosquitto over WebSocket. The GCS subscribes in-browser via mqtt.js. 2Hz+ update rate.

**RTSP video.** The video pipeline pushes to a cloud relay, which converts it to fMP4-over-WebSocket for browser playback at 0.5-1.5s latency.

| Config field | Default | Description |
|---|---|---|
| `server.mode` | `disabled` | `disabled`, `cloud`, or `self_hosted` |
| `server.mqtt_transport` | `tcp` | `tcp` or `websockets` |
| `server.mqtt_username` | - | MQTT broker username |
| `video.cloud_relay_url` | - | RTSP relay server URL |

---

## Architecture

```
              ┌───────────────────────────────┐
   CLI  ────▶ │   REST API  ·  setup webapp   │ ◀──── Mission Control / HTTP clients
              │   Python (FastAPI) on :8080   │
              └───────────────┬───────────────┘
                              │ Unix-socket IPC + shared state
   ┌──────────────────────────┴────────────────────────────────┐
   │              Rust core services (systemd units)             │
   │   supervisor · MAVLink router · video · cloud relay ·       │
   │   WFB-ng radio · vision · ground-station receive / uplink   │
   └────┬───────────────────┬────────────────────┬──────────────┘
        ▼                   ▼                    ▼
  flight controller    RTL8812EU + wfb (C)    camera / NPU
  (serial / USB)       ffmpeg · mediamtx      (Python + C)
```

The Rust supervisor owns process lifecycle and orchestrates systemd. Python keeps the REST surface, AI and vision inference, hardware detection, scripting, and the plugin runtime. Routes read FC status, telemetry, video, radio, and parameter state through named accessors, so the API stays independent of service internals.

### Directory Structure

| Path | Contents |
|------|----------|
| `crates/` | Native Rust services (one crate per service) plus the shared protocol and SDK crates |
| `src/ados/` | Python runtime: REST API, HAL detection, scripting, plugins, SDK, ground-station managers |
| `data/` | systemd units, udev rules, overlays, and other deploy-time assets |
| `scripts/` | Installer bootstrap and helper scripts |
| `dashboard/` | Local web dashboard assets |
| `docs/` | Deployment, ground-station, and OEM documentation |
| `tests/` | Test suite |

---

## What's Working

| Feature | Status |
|---------|--------|
| Rust process supervisor (systemd orchestration) | Working |
| MAVLink router (serial to WS/TCP/UDP) | Working |
| REST API and universal setup webapp (Python FastAPI) | Working |
| Prebuilt Rust installer (fetch, verify, provision, configure) | Working |
| Minimal public CLI | Working |
| Demo mode (simulated telemetry) | Working |
| Hardware auto-detection (board profiles) | Working |
| Config system (Pydantic + YAML) | Working |
| Health monitoring (CPU, RAM, disk, temp) | Working |
| Cloud relay (Convex HTTP + MQTT) | Working |
| GCS pairing (local-first over LAN, cloud relay optional) | Working |
| OTA updates (upgrade + rollback) | Working |
| Video pipeline (RTSP, WebRTC/WHEP, WFB-ng) | Working |
| WFB-ng long-range link (5.8 GHz, FEC) | Working |
| On-device vision host (frame bus, detectors) | Working |
| Script executor (text commands + Python SDK) | Working |
| Plugin system (Python and Rust plugins) | Working |
| Ground-station distributed receive and local mesh | Working |
| Swarm formation flight | Planned |

---

## Development

The Python runtime:

```bash
git clone https://github.com/altnautica/ADOSDroneAgent.git
cd ADOSDroneAgent
python -m venv .venv && source .venv/bin/activate
pip install -e ".[dev]"

pytest          # run tests
ruff check src/ # lint
ados --help     # inspect the public CLI
```

The native services live under `crates/` (a Cargo workspace):

```bash
cargo build     # build the Rust service crates
cargo test
```

On a device the installer fetches prebuilt Rust binaries, so deploying needs no Rust toolchain. You only need one to build the services from source.

API route changes go through the runtime facade in `ados.api.runtime`. Tests that need runtime doubles use `tests/api_runtime_utils.py`. See [AGENTS.md](AGENTS.md) for architecture notes and [CONTRIBUTING.md](CONTRIBUTING.md) for code style and PR guidelines.

---

## Hardware Partners

Building and testing ADOS Drone Agent on real companion computers and flight hardware. Want to get involved? [Email us](mailto:team@altnautica.com).

<!-- Format: | [![Company](logo-url)](website) -->

*Interested in sponsoring or sending test hardware? See our [partnership info](mailto:team@altnautica.com).*

---

## Community

- **[Discord](https://discord.gg/uxbvuD4d5q)** - Ask questions, share builds
- **[LinkedIn](https://www.linkedin.com/company/altnautica/)** - Follow company updates
- **[Email](mailto:team@altnautica.com)** - team@altnautica.com
- **[Issues](https://github.com/altnautica/ADOSDroneAgent/issues)** - Bug reports and discussions
- **[Website](https://altnautica.com)** - Company and product info

---

## Related

- [ADOS Mission Control](https://github.com/altnautica/ADOSMissionControl) - browser GCS (the control side of this pair)
- [ADOS Extensions](https://github.com/altnautica/ADOSExtensions) - first-party plugins for the agent and the GCS

---

## License

[GPL-3.0-only](LICENSE). Free to use, modify, and distribute. Derivative works must also be GPL-3.0.
