# ADOS Drone Agent

**Open-source onboard agent for software-defined drones. Long-range data link. HD video. Full remote control.**

![License: GPL-3.0](https://img.shields.io/badge/License-GPL--3.0-green.svg) ![Rust + Python](https://img.shields.io/badge/Core-Rust%20%2B%20Python-blue.svg) ![Status: Alpha](https://img.shields.io/badge/Status-Alpha-orange.svg) [![Discord](https://img.shields.io/badge/Discord-Join-5865F2.svg)](https://discord.gg/uxbvuD4d5q)

ADOS Drone Agent is the onboard software for a software-defined drone. It runs on the companion computer next to your flight controller, reads MAVLink off the FC and routes it to any ground station, streams HD video over radio or the internet, and lets you fly and manage the aircraft from a browser. The flight-critical paths are native Rust. AI, vision, and plugins run in Python.

> **Part of the ADOS ecosystem.** Pairs with [ADOS Mission Control](https://github.com/altnautica/ADOSMissionControl) (the browser ground station) for mission planning, 3D simulation, AI PID tuning, and gamepad flight control. The agent runs on the drone; Mission Control runs in your browser. Extend either side with [ADOS Extensions](https://github.com/altnautica/ADOSExtensions), the first-party plugin repo.

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
      <sub>Overview, showing running services, system resources, and live logs in Mission Control</sub>
    </td>
    <td width="50%">
      <img src="docs/screenshots/fleet-network.png" alt="Fleet network enrollment, MQTT gateway, mesh radio peers" height="220" width="100%"><br>
      <sub>Fleet network enrollment, MQTT gateway status, and mesh radio peers</sub>
    </td>
  </tr>
  <tr>
    <td width="50%">
      <img src="docs/screenshots/peripherals.png" alt="Connected peripherals with live sensor readings" height="220" width="100%"><br>
      <sub>Connected peripherals with live sensor readings; drivers extend through the plugin system</sub>
    </td>
    <td width="50%"></td>
  </tr>
</table>

---

## What it lets you do

- **Turn a companion computer into a connected drone with one command.** A single installer flashes nothing, builds nothing on the device, and leaves a paired, video-streaming, telemetry-flowing aircraft.
- **Pair over your own network, no cloud account.** The agent auto-pairs with Mission Control over the LAN. Cloud relay is there when you want to reach the drone from anywhere, and off by default.
- **Fly the FC from a browser.** Arm, change modes, fly guided, and upload missions. The agent routes MAVLink to WebSocket, TCP, and UDP at the same time, so several ground stations can connect at once.
- **Watch HD video over radio or the internet.** The video pipeline drives RTSP, WebRTC (WHEP), and a long-range WFB-ng radio link, and plays back in the browser.
- **Reach the drone from anywhere.** Optional cloud relay carries telemetry and commands over the internet with no port forwarding.
- **Sign your MAVLink.** Transparent pass-through for MAVLink v2 signing, with one-shot flight-controller enrollment. Key material stays in the ground station, never on the agent.
- **Run the same agent on the ground.** A ground-station profile (no flight controller, an OLED and buttons, a WFB-ng adapter) turns a small board into the receiving end of the radio link.
- **Build distributed receive.** Two or three ground nodes form a self-healing mesh so you keep the link in obstructed areas.
- **Extend it without forking it.** Install signed plugins that add drivers, behaviors, AI models, and ground-station hardware support.

## Quick Start

Deploy to a companion computer (Raspberry Pi, Radxa, Jetson, and similar ARM64 Linux boards):

```bash
curl -sSL https://raw.githubusercontent.com/altnautica/ADOSDroneAgent/main/scripts/install.sh | sudo bash
```

Root is required. The one-line bootstrap fetches and verifies a prebuilt Rust installer, which places the native Rust service binaries, provisions a Python virtualenv for the feature services, configures the systemd units, and starts the setup webapp on the device.

After install, SSH into the node and run:

```bash
ados
```

The terminal status page shows the local setup URL, LAN or hotspot URLs, MAVLink state, video state, services, and remote-access state.

### System Requirements

| Requirement | Minimum | Recommended |
|-------------|---------|-------------|
| OS | Linux with systemd (ARM64 or x86_64) | Raspberry Pi OS, Debian, Ubuntu, Armbian |
| RAM | 1 GB | 2 GB+ |
| Storage | 500 MB | 2 GB+ |
| Python | 3.11+ | 3.12 |
| FC connection | Serial (UART or USB) | UART at 921600 baud |

Also runs on macOS for local development (the installer points you to `cargo` there). Boards with less than 1 GB of RAM are not supported by the standard profile.

---

## Why Rust

The flight-critical paths are native Rust: the process supervisor that drives systemd, the MAVLink router, the video pipeline, the WFB-ng radio control, the on-device vision host, and the HTTP control surface on `:8080`. That choice buys three things you can feel:

- **Predictable, low-overhead control and telemetry.** No garbage-collector pauses on the path between the flight controller and the ground.
- **A small memory and CPU footprint.** The native services idle in tens of megabytes, which leaves headroom for video encode and vision on modest boards.
- **A path to a zero-Python headless profile.** On the smallest boards the agent can run the Rust core alone (MAVLink, camera, radio, control), which drops the Python interpreter and most of the memory floor with it.

Python stays where it earns its place: AI and vision inference, the plugin runtime, hardware bring-up, and the setup webapp. A few mature C programs are supervised rather than rewritten (the RTL8812EU driver, `wfb_tx` and `wfb_rx`, `ffmpeg`, and `mediamtx`). systemd remains the process manager; the Rust supervisor orchestrates the units, it does not replace systemd.

---

## Architecture at a glance

`ados-control`, a native Rust HTTP front, owns port `:8080`. It serves the control, telemetry, pairing, parameter, fleet, and ground-station routes natively, and reverse-proxies a small Python feature service on an internal socket for the parts that stay in Python: AI and vision inference, the plugin runtime, the setup webapp, and WHEP video.

```
              ┌────────────────────────────────────┐
   CLI  ────▶ │  HTTP control surface  ·  :8080     │ ◀──── Mission Control / HTTP clients
              │  ados-control (native Rust front)   │
              └───────────────┬────────────────────┘
                              │ proxies the Python feature
                              │ service on an internal socket
   ┌──────────────────────────┴────────────────────────────────┐
   │              Rust core services (systemd units)             │
   │   supervisor · MAVLink router · video · cloud relay ·       │
   │   WFB-ng radio · vision · ground-station receive / uplink   │
   └────┬───────────────────┬────────────────────┬──────────────┘
        ▼                   ▼                     ▼
  flight controller   RTL8812EU + wfb (C)    camera / NPU
  (serial / USB)      ffmpeg · mediamtx       (Python + C)

  Python feature service: AI + vision inference · plugin runtime ·
                          setup webapp · WHEP video
```

On a device the agent installs as a set of systemd services plus a Python virtualenv, both placed by the prebuilt Rust installer. Routes read FC status, telemetry, video, radio, and parameter state through named accessors, so the control surface stays independent of service internals.

---

## Hardware Support

The agent auto-detects the board on boot and scales features to available resources. Profiles for 17 boards ship in `src/ados/hal/boards/`, and any ARM64 or x86_64 Linux board with a serial port and systemd should work through the `generic-arm64` profile.

| Class | Example boards | Capabilities |
|-------|----------------|--------------|
| Entry | Raspberry Pi 3, Radxa CM3 (RK3566) | MAVLink proxy, cloud relay, telemetry |
| Standard | Raspberry Pi 4 / 5, CM4, Orange Pi 5 | + HD video pipeline, WFB-ng radio |
| Accelerated | CM5, Rock 5C Lite, Cubie A7Z, RK3576 / RK3588S2, Jetson Nano / Orin Nano | + on-device vision, NPU-backed inference, plugin sandbox |

The same codebase also runs the ground-station profile (no flight controller, an OLED or SPI LCD with buttons, and a WFB-ng adapter); a hardware fingerprint at boot picks the profile.

**Mesh role hardware.** A single ground node (`direct` role) needs one RTL8812EU USB WiFi adapter for WFB-ng. Relay and receiver nodes add a second USB WiFi dongle that carries the batman-adv mesh traffic between nodes. Any adapter with a Linux driver that supports 802.11s or IBSS mode works for the mesh carrier.

---

## CLI Reference

The public everyday CLI is intentionally small. Run `ados --help` for the current command list.

| Command | Description |
|---------|-------------|
| `ados` | Open the read-only terminal status page, or plain status when not attached to a TTY |
| `ados status` | Print setup, MAVLink, video, network, remote-access, and service status |
| `ados status --json` | Print the full setup facade payload for scripts |
| `ados update` | Update the agent to the latest version (re-runs the installer upgrade) |
| `ados update --check-only` | Report the current and latest version without installing |
| `ados uninstall` | Remove the agent from the system |
| `ados uninstall --purge --yes` | Remove services, package files, and config without prompts |

Grouped subcommands cover the deeper surfaces: `ados plugin` (install and manage plugins), `ados logs` (query the on-device black-box log store), `ados network`, `ados radio`, and `ados profile`. Use the setup webapp or the Mission Control node panel for configuration, ground-station pairing, video, network, and remote access.

---

## REST API

Served by the native Rust front (`ados-control`) on `:8080`. The control and telemetry routes are native; AI/vision, plugin, setup, and WHEP routes are proxied to the Python feature service. Full reference at [docs.altnautica.com](https://docs.altnautica.com).

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/api/v1/setup/status` | GET | Universal setup, access URL, MAVLink, video, network, and remote-access status |
| `/api/v1/setup/remote-access/cloudflare` | POST | Install a Cloudflare Tunnel token or install command |
| `/api/status` | GET | Agent status, uptime, FC state |
| `/api/telemetry` | GET | Attitude, GPS, battery snapshot |
| `/api/params` | GET | Read cached FC parameters |
| `/api/command` | POST | Send a MAVLink command to the FC |
| `/api/commands` | GET | List supported command names |
| `/api/config` | GET / PUT | Read or update agent config |
| `/api/services` | GET | Running services and status |
| `/api/video` | GET / POST | Video pipeline status and control |
| `/api/plugins` | GET / POST / DELETE | Plugin install, list, enable, disable, remove, info |
| `/api/fleet/*` | GET | Fleet enrollment and peer status |
| `/api/peripherals` | GET | Connected sensors and hardware |
| `/api/pairing/*` | GET / POST | GCS pairing management |
| `/api/mavlink/signing/*` | GET / POST | Signing capability detection and one-shot FC enrollment |
| `/api/system` | GET / POST | System info, reboot, shutdown |
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

## Connectivity

ADOS is local-first. The primary path between Mission Control and the agent is your own network.

**Local pairing (default).** Mission Control reaches the agent over the LAN by hostname or IP. The apiKey is stored in the browser, with no cloud round-trip. An agent with cloud relay disabled is in its normal, correct state.

**Long-range radio link.** The drone-to-ground data link is WFB-ng on 5.8 GHz, with forward error correction, for video and telemetry without any internet. It auto-binds to the ground station on first boot.

**Cloud relay (optional).** For remote access across networks, the agent connects to Mission Control over a relay:

- **Convex HTTP (baseline).** Every 5 seconds the agent posts full status to the cloud, and the ground station reads it through reactive queries. Commands go the other way. No extra infrastructure.
- **MQTT telemetry (real-time).** When `server.mode` is `cloud` or `self_hosted`, the agent publishes to `ados/{deviceId}/status` and `ados/{deviceId}/telemetry` over a Mosquitto WebSocket. The browser subscribes directly. 2 Hz and faster.
- **RTSP video.** The video pipeline pushes to a cloud relay that converts it to fMP4 over WebSocket for browser playback at 0.5 to 1.5 s latency.

| Config field | Default | Description |
|---|---|---|
| `server.mode` | `disabled` | `disabled`, `cloud`, or `self_hosted` |
| `server.mqtt_transport` | `tcp` | `tcp` or `websockets` |
| `server.mqtt_username` | - | MQTT broker username |
| `video.cloud_relay_url` | - | RTSP relay server URL |

---

## Directory Structure

| Path | Contents |
|------|----------|
| `crates/` | Native Rust services (one crate per service) plus the shared protocol and SDK crates |
| `src/ados/` | Python runtime: feature service, HAL detection, plugins, SDK, ground-station managers |
| `data/` | systemd units, udev rules, overlays, and other deploy-time assets |
| `scripts/` | Installer bootstrap and helper scripts |
| `dashboard/` | Local web dashboard assets |
| `docs/` | Deployment, ground-station, and OEM documentation |
| `tests/` | Test suite |

---

## What's Working

| Feature | Status |
|---------|--------|
| Native Rust HTTP front (`ados-control` on `:8080`) | Working |
| Rust process supervisor (systemd orchestration) | Working |
| MAVLink router (serial to WS/TCP/UDP) | Working |
| Prebuilt Rust installer (fetch, verify, provision, configure) | Working |
| Minimal public CLI | Working |
| Demo mode (simulated telemetry) | Working |
| Hardware auto-detection (board profiles) | Working |
| Config system (Pydantic + YAML) | Working |
| Health monitoring (CPU, RAM, disk, temp) | Working |
| On-device black-box log store (`ados logs`) | Working |
| Cloud relay (Convex HTTP + MQTT) | Working |
| GCS pairing (local-first over LAN, cloud relay optional) | Working |
| MAVLink v2 signing pass-through | Working |
| Agent updates (`ados update`) | Working |
| Video pipeline (RTSP, WebRTC/WHEP, WFB-ng) | Working |
| WFB-ng long-range link (5.8 GHz, FEC, auto-bind) | Working |
| On-device vision host (frame bus, detectors) | Working |
| Plugin system (Python and Rust plugins) | Working |
| Ground-station distributed receive and local mesh | Working |
| Lean headless profile (Rust core only) | In progress |
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

See [AGENTS.md](AGENTS.md) for architecture notes and [CONTRIBUTING.md](CONTRIBUTING.md) for code style and PR guidelines.

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

- [ADOS Mission Control](https://github.com/altnautica/ADOSMissionControl) - browser ground station (the control side of this pair)
- [ADOS Extensions](https://github.com/altnautica/ADOSExtensions) - first-party plugins for the agent and the GCS

---

## License

[GPL-3.0-only](LICENSE). Free to use, modify, and distribute. Derivative works must also be GPL-3.0.
