# AGENTS.md - ADOS Drone Agent

Agentic coding instructions for ADOS Drone Agent, the open-source Python agent
for drone and ground-station Linux nodes.

## Stack and Commands

- Python 3.11+, FastAPI, Click, Pydantic, Textual, and systemd-oriented
  services.
- Package source lives under `src/ados/`.
- Version source of truth: `src/ados/__init__.py`.
- Common commands:

```bash
pip install -e ".[dev]"
pytest
ruff check .
mypy src
ados demo
ados status
```

Use the CLI before raw shell operations when an agent command exists.

## Architecture Guidelines

- Keep runtime behavior in source, installer, config templates, or service
  definitions. Do not rely on manual edits to installed runtime files.
- The installer must be repeatable and automatic. If setup needs a manual
  follow-up command, fix the installer or agent code.
- Hardware and service detection should degrade cleanly when optional devices
  are absent.
- Keep API schemas typed with Pydantic models. Avoid loose dictionaries at new
  public API boundaries when a model belongs there.
- CLI commands should be idempotent where practical and safe to run over SSH.
- Plugin and extension code must enforce declared permissions before handler
  logic runs.

## Deployment Discipline

Changes flow from this repository into installed nodes through the install or
upgrade path. Read logs and service status for diagnostics, but put fixes back
into source and installer code.

Do not patch installed files under `/opt`, `/etc`, or systemd unit directories
as a substitute for repo changes.

## Testing Expectations

Add or update tests for service managers, config migration, plugin permissions,
API routes, and CLI behavior touched by a change. Keep tests deterministic and
hardware-aware code mockable.

## Repository Boundary

Keep repo instructions, docs, comments, tests, and examples self-contained and
technical. Document behavior through code architecture, APIs, commands, config
schemas, service definitions, hardware interfaces, deployment steps, and
operator workflows. Keep this repository self-contained. Describe integrations
through documented APIs, package names, public protocols, and public project
links.

## Related Public Projects

- [ADOS Mission Control](https://github.com/altnautica/ADOSMissionControl) -
  browser ground control station that can connect to this agent.
- [ADOSExtensions](https://github.com/altnautica/ADOSExtensions) - plugin
  extensions built for the ADOS plugin system.
- [ADOS Documentation](https://github.com/altnautica/Documentation) - public
  docs for installation, APIs, and operator workflows.
