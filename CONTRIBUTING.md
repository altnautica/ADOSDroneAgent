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

The agent is a systemd-managed process supervisor with modular services:

```
src/
  core/           # Process supervisor, config, logging
  services/
    mavlink/      # MAVLink proxy and routing
    video/        # WFB-ng video pipeline management
    mqtt/         # MQTT gateway for fleet telemetry
    suite/        # Suite runtime (YAML manifest execution)
    script/       # Script executor (text commands, Python SDK)
    ota/          # Over-the-air update manager
    sensor/       # Sensor discovery and management
  plugins/        # Plugin system (Python entry points)
  sdk/            # Python SDK (ados package)
  api/            # REST API (FastAPI)
```

Each service runs as a systemd unit. Plugins extend functionality via Python entry points.

## License

By contributing, you agree that your contributions will be licensed under the GPLv3 license.

## Community

- GitHub Issues: [altnautica/ADOSDroneAgent/issues](https://github.com/altnautica/ADOSDroneAgent/issues)
- ADOS Mission Control community: [command.altnautica.com/community](https://command.altnautica.com/community)
- ArduPilot Discuss: [discuss.ardupilot.org](https://discuss.ardupilot.org/)
