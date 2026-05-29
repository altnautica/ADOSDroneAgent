# Contributing to ADOS Drone Agent

Thanks for your interest in contributing to the ADOS Drone Agent. This document covers the development setup, code style, and contribution process.

## Development Setup

### Prerequisites

- Python 3.11 or later
- A Linux SBC (Raspberry Pi CM4/CM5, Jetson Nano, or similar) for testing
- ArduPilot SITL (optional, for simulation without hardware)

### Install

```bash
git clone https://github.com/altnautica/ADOSDroneAgent.git
cd ADOSDroneAgent
python -m venv .venv
source .venv/bin/activate
pip install -e ".[dev]"
```

### Running Tests

```bash
pytest
```

### Linting and Formatting

We use Ruff for linting and Black for formatting.

```bash
ruff check .
black .
mypy src/
```

## Code Style

- **Formatter:** Black (default settings, 100 char line length)
- **Linter:** Ruff (E, F, I, N, W, UP rules)
- **Type checker:** mypy (strict mode)
- **Docstrings:** Google style
- **Imports:** sorted by isort (via Ruff)

## Pull Request Process

1. Fork the repository
2. Create a feature branch (`git checkout -b feature/your-feature`)
3. Make your changes
4. Run the test suite (`pytest`)
5. Run linting (`ruff check . && mypy src/`)
6. Commit with a clear message describing the change
7. Push to your fork and open a Pull Request

### PR Guidelines

- Keep PRs focused on a single change
- Include tests for new functionality
- Update documentation if behavior changes
- Reference any related issues in the PR description

## Issue Templates

When opening an issue, please include:

- **Bug reports:** Steps to reproduce, expected behavior, actual behavior, hardware/OS info
- **Feature requests:** Use case description, proposed solution (if any), alternatives considered

## Architecture Overview

The agent is a hybrid Rust/Python systemd-managed process supervisor with
modular services. Long-running and safety-critical services (process
supervision, MAVLink routing, video pipeline, cloud relay, networking,
radio/WFB, display/HID) run as native Rust services. Python handles the REST
API, AI/ML inference, HAL detection, drivers, scripting, and the plugin system.

```
crates/           # Native Rust services (one crate per service)
  ados-supervisor/        # Process supervisor / systemd orchestration
  ados-mavlink-router/    # MAVLink proxy and routing
  ados-video/             # Video pipeline management
  ados-cloud/             # Cloud relay (Convex HTTP + MQTT)
  ados-radio/             # WFB-ng radio control
  ados-groundlink/        # Ground-station receive and mesh
  ados-net/               # Ground-station uplink matrix
  ados-display/           # Physical UI (OLED / SPI LCD)
  ados-hid/               # Buttons and joystick input
  ados-plugin-host/       # Plugin host runtime
  ados-protocol/ ados-sdk/ ados-tui/ ados-capabilities-codegen/

src/                # Python runtime
  core/           # Config, logging, IPC
  services/
    scripting/    # Script executor (text commands, Python SDK)
    ota/          # Over-the-air update manager
    sensor/       # Sensor discovery and management
    vision/       # Vision model registry
  hal/            # Hardware detection and board profiles
  plugins/        # Plugin system (Python entry points)
  sdk/            # Python SDK (ados package)
  api/            # REST API (FastAPI)
```

Each service runs as a systemd unit. Plugins extend functionality via Python or
Rust entry points through the plugin host.

## License

By contributing, you agree that your contributions will be licensed under the GPLv3 license.

## Community

- GitHub Issues: [altnautica/ADOSDroneAgent/issues](https://github.com/altnautica/ADOSDroneAgent/issues)
- Discord: [discord.gg/uxbvuD4d5q](https://discord.gg/uxbvuD4d5q)
- ArduPilot Discuss: [discuss.ardupilot.org](https://discuss.ardupilot.org/)
