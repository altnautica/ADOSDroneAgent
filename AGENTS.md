# AGENTS.md - ADOS Drone Agent

Agentic coding instructions for ADOS Drone Agent, the open-source Python agent
for drone companion computers and ground-station Linux nodes.

## Purpose

Work in this repository as an engineering agent for the Python runtime, CLI,
API, services, installer, HAL profiles, and plugin host. Keep changes
deterministic, typed, testable without hardware where possible, and safe to
apply through the normal install or upgrade path.

This file is self-contained for public repository work. Do not rely on
instructions outside this repository when writing code, docs, comments, tests,
examples, logs, or commit messages here.

## Read First

- Check `git status --short` before edits and preserve unrelated changes.
- Inspect nearby service, CLI, API, config, HAL, or plugin patterns before
  adding new structure.
- Use the `ados` CLI before raw shell operations when an agent command exists.
- Keep the hidden no-hardware demo path working for local verification.
- Put fixes in repository source, installer code, config templates, or service
  definitions. Do not treat installed runtime edits as the fix.
- Bump `src/ados/__init__.py` when a shipped behavior change is intended.

## Stack and Commands

- Python 3.11+, FastAPI, Click, Pydantic, Rich, structlog, and
  systemd-oriented services.
- Package source lives under `src/ados/`.
- Version source of truth: `src/ados/__init__.py`.
- Common commands:

```bash
pip install -e ".[dev]"
pytest
ruff check .
mypy src
ados status
```

- Useful focused commands:

```bash
pytest tests/path/to/test_file.py
pytest tests/path/to/test_file.py -k test_name
ruff check src/ados/path tests/path
mypy src
ados --help
ados --help
```

Use `python3` for one-off local scripts when a Python command is needed.

## Architecture Map

- CLI: `src/ados/cli/`
- FastAPI app and routes: `src/ados/api/`
- Core runtime and supervisor: `src/ados/core/`
- Services: `src/ados/services/`
- Ground-station services: `src/ados/services/ground_station/`
- HAL and board profiles: `src/ados/hal/`
- Built-in plugins and runner: `src/ados/plugins/`
- SDK and test helpers: `src/ados/sdk/`
- Web assets served by the agent: `src/ados/webapp/`
- Setup facade and terminal status data: `src/ados/setup/`
- Tests: `tests/`

Keep files near 300 lines when practical. Split before modules become hard to
review, except generated files, fixtures, data tables, and vendored code.

## Coding Rules

- Keep public API boundaries typed with Pydantic models. Avoid loose dictionaries
  when a request or response model belongs there.
- Keep hardware-aware code mockable and deterministic in tests.
- Hardware and service detection should degrade cleanly when optional devices
  are absent.
- CLI commands should be idempotent where practical and safe over SSH.
- Plugin and extension code must enforce declared permissions before handler
  logic runs.
- Config migration and installer changes must be repeatable. If setup needs a
  manual follow-up command, fix the installer or agent code.
- Prefer explicit errors and structured logs that help operators diagnose state
  without exposing environment-specific details.

## Runtime and Deployment Discipline

Changes flow from this repository into installed nodes through the install or
upgrade path. Read logs, service status, and health output for diagnostics, then
put the fix back into source.

Do not patch installed files under `/opt`, `/etc`, or systemd unit directories
as a substitute for repository changes.

Service, installer, and HAL changes should fail safely when dependencies,
hardware devices, interfaces, or permissions are unavailable.

## Public Boundary

Keep this repository self-contained and technical. Document behavior through
architecture, APIs, commands, config schemas, service definitions, hardware
interfaces, deployment steps, and operator workflows.

Do not include non-public company context, named customers, financial context,
internal planning labels, attribution trails, or source-path hints from outside
this repository. Use neutral placeholders such as `example-oem`,
`cloud.example.com`, and public protocol names.

Comments, examples, fixtures, test names, logs, errors, PR titles, and commit
messages should be bland and technical. Do not write messages that describe a
cleanup of sensitive wording.

## Verification

- CLI behavior: add or update Click tests, then run the focused pytest target
  and a bounded `ados ... --help` or demo smoke when practical.
- API routes: test request and response models plus failure paths.
- Services, config migration, installer, HAL, and plugin permissions: add or
  update deterministic tests around the touched behavior.
- Typed Python changes: run `ruff check .` and `mypy src` when the touched code
  affects shared types, public APIs, services, or plugin contracts.
- Hardware-adjacent changes: verify no-hardware fallback behavior in tests or
  demo mode.

Before finalizing, run `git diff --check` and targeted scans on changed public
files for non-public context, named customers, internal planning labels,
attribution-trail wording, and financial context. Report any skipped checks.

## Review Expectations

When reviewing, list findings first and focus on runtime regressions, unsafe
installer behavior, service lifecycle bugs, permission bypasses, hardware
fallback gaps, untyped API boundaries, missing tests, and CLI UX defects. Cite
file and line references.

For implementation work, keep fixes in source and verification focused on the
behavior changed.

## Cross-Repo Impact

- API, telemetry, health, and capability changes may require Mission Control UI
  handling and generated client types.
- Setup, CLI, config, and troubleshooting changes may require Documentation
  updates.
- Plugin host or permission changes may require ADOSExtensions manifest and SDK
  compatibility checks.

## Related Public Projects

- [ADOS Mission Control](https://github.com/altnautica/ADOSMissionControl) -
  browser ground control station that can connect to this agent.
- [ADOSExtensions](https://github.com/altnautica/ADOSExtensions) - plugin
  extensions built for the ADOS plugin system.
- [ADOS Documentation](https://github.com/altnautica/Documentation) - public
  docs for installation, APIs, and operator workflows.
