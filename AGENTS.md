# AGENTS.md - ADOS Drone Agent

Agentic coding instructions for ADOS Drone Agent, the open-source hybrid
Rust/Python agent for drone companion computers and ground-station Linux nodes.

## Purpose

Work in this repository as an engineering agent across both halves: the Rust
crate workspace under `crates/`, which owns the flight-critical services, and
the Python runtime under `src/ados/`, which owns AI and vision inference, the
HAL bootstrap glue, the plugin runtime, the SDK, and the residual HTTP surface.
The CLI, the installer, HAL profiles and the plugin host each have a half on
both sides, so check which one you are in before you start. Keep changes
deterministic, typed, testable without hardware where possible, and safe to
apply through the normal install or upgrade path.

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

- Rust, stable toolchain, MSRV 1.88. The workspace manifest is
  `crates/Cargo.toml` with 31 members; `cargo` must be run from `crates/`,
  there is no manifest at the repository root.
- Python 3.11+, Click, Pydantic, Rich, structlog, and systemd-oriented
  services. FastAPI is the residual HTTP surface that runs behind the native
  front, not the primary API layer.
- Package source lives under `src/ados/`; crate source lives under
  `crates/<crate-name>/src/`.
- Version source of truth: `src/ados/__init__.py`.
- Common commands:

```bash
# Python half
pip install -e ".[dev]"
pytest
ruff check .
mypy src/ados/<touched-module>
ados status

# Rust half, run from crates/
cd crates
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build --release --all-features
```

- Useful focused commands:

```bash
pytest tests/path/to/test_file.py
pytest tests/path/to/test_file.py -k test_name
ruff check src/ados/path tests/path
mypy src/ados/<touched-module>
ados --help

# Rust, scoped to one crate, still from crates/
cargo test -p ados-video
cargo clippy -p ados-video --all-targets -- -D warnings
```

Use `python3` for one-off local scripts when a Python command is needed.

`ruff check .` is clean and is the lint gate; `vendor/` is excluded because that
tree carries third-party driver and radio sources verbatim. A whole-tree
`mypy src` still reports a large pre-existing backlog, so type-check the modules
you touched and do not treat the full run as a gate.

## Architecture Map

Rust, under `crates/`, 31 workspace members. The flight-critical path:

- Service supervision: `crates/ados-supervisor/`
- MAVLink routing: `crates/ados-mavlink-router/`
- Video pipeline: `crates/ados-video/`
- Radio and link control: `crates/ados-radio/`, `crates/ados-rate-control/`
- HTTP control surface, the native front on `:8080`: `crates/ados-control/`
- Logging and telemetry store: `crates/ados-logd/`
- Networking: `crates/ados-net/`, `crates/ados-macpin/`
- Cloud relay: `crates/ados-cloud/`
- Ground-station link and mesh: `crates/ados-groundlink/`
- Installer: `crates/ados-installer/`
- Plugin host: `crates/ados-plugin-host/`
- Vision engine: `crates/ados-vision/`
- World model and offload: `crates/ados-atlas/`,
  `crates/ados-atlas-transport/`, `crates/ados-offload/`,
  `crates/ados-compute/`
- Swarm: `crates/ados-swarm-control/`, `crates/ados-swarmbus/`
- Peripherals: `crates/ados-crsf/`, `crates/ados-display/`,
  `crates/ados-hid/`, `crates/ados-gpio/`, `crates/ados-hal-probe/`
- Shared: `crates/ados-protocol/` (wire contracts and their version
  registry, `contracts.toml`), `crates/ados-config/`, `crates/ados-sdk/`,
  `crates/ados-capabilities-codegen/`, `crates/ados-config-tunnel/`,
  `crates/ados-tui/`

Python, under `src/ados/`:

- CLI: `src/ados/cli/`
- Residual HTTP surface behind the native front: `src/ados/api/`
- Core runtime: `src/ados/core/` (config writer, IPC, paths)
- Services: `src/ados/services/`
- Ground-station services: `src/ados/services/ground_station/`
- HAL and board profiles: `src/ados/hal/`
- Built-in plugins and runner: `src/ados/plugins/`
- SDK and test helpers: `src/ados/sdk/`
- Dashboard SPA: `src/ados/dashboard/`
- Cockpit SPA: `src/ados/cockpit/`
- Bootstrap and profile detection: `src/ados/bootstrap/`
- Compute service: `src/ados/compute/`
- Security (HMAC, certs, firewall): `src/ados/security/`
- Data files (plugin catalog, param metadata): `src/ados/data/`
- Setup facade and terminal status data: `src/ados/setup/`
- Tests: `tests/`

`docs/api-surface.md` is the generated table of every HTTP route and which
half serves it. Do not hand-edit it; regenerate with
`scripts/gen-api-surface.py`. Two guards keep it true: a crate test fails
when the native section drifts from the router, and
`scripts/check-api-surface.py` fails when a client calls a path the table
does not carry.

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

## Working in the Open

This is a public, open-source repository. Every commit, diff, and branch is
visible the moment it is pushed and stays in history permanently, so a mistake
cannot be un-published by deleting it later. Review what a change actually
contains before committing.

- **Never commit secrets.** API keys, tokens, deploy keys, passwords, private
  certificates, and `.env` files stay out of the tree. Generated secrets belong
  only in gitignored files. If a secret does land in a commit, treat it as
  compromised and rotate it.
- **Never commit real deployment detail.** Hostnames, IP addresses, tunnel
  names, device identifiers, and account names from a live setup are an attack
  surface. Use placeholders such as `example-oem`, `cloud.example.com`,
  `192.168.1.50`, and `mycompany-fleet`.
- **Never commit other people's data.** Personal names, email addresses,
  customer or employer names, real flight logs and GPS traces, and raw log
  dumps that contain any of the above do not belong in a public repository.
- **Tests are published too.** Fixtures, `parametrize` tables, sample YAML and
  JSON, HAL board profiles, and systemd unit descriptions get the same care as
  source.
- **Respect licensing when bringing in outside code.** Third-party source is
  vendored into a vendor directory with its license intact and is never pasted
  into our own modules.
- **Keep contributions technical.** Architecture, APIs, commands, schemas,
  configuration, hardware interfaces, deployment, and troubleshooting.
  Commercial, pricing, or roadmap commentary does not belong in the codebase.
- **Comments, log strings, commit messages, and PR titles are public too.** Keep
  them bland, factual, and technical.

## Verification

- CLI behavior: add or update Click tests, then run the focused pytest target
  and a bounded `ados ... --help` or demo smoke when practical.
- API routes: test request and response models plus failure paths.
- Services, config migration, installer, HAL, and plugin permissions: add or
  update deterministic tests around the touched behavior.
- Typed Python changes: run `ruff check .` and
  `mypy src/ados/<touched-module>` when the touched code affects shared types,
  public APIs, services, or plugin contracts. The whole-tree `mypy src` has a
  known pre-existing backlog and is not a gate (see Stack and Commands).
- Rust changes: from `crates/`, run `cargo fmt --check`, `cargo clippy -p
  <crate> --all-targets -- -D warnings`, and `cargo test -p <crate>`. A change
  to a wire contract or a capability also needs `cargo test -p ados-protocol`
  and the codegen drift check, because the generated Rust, Python and
  TypeScript types are asserted against `crates/ados-protocol/contracts.toml`.
- Hardware-adjacent changes: verify no-hardware fallback behavior in tests or
  demo mode.

Before finalizing, run `git diff --check` and report any skipped checks.

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
