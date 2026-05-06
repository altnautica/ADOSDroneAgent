# Changelog

All notable changes to the ADOS Drone Agent are recorded here.
Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
the project follows [Semantic Versioning](https://semver.org/).

## [0.12.9] - 2026-05-07

### Added

- **Drone-profile dashboard panels.** Live video (WebRTC primary, HLS
  fallback, MJPEG snapshot last-resort, fullscreen and snapshot
  verbs), flight controller (vehicle, firmware, mode, armed, GPS, RC,
  battery, link, prearm, 60s link sparkline), MAVLink rates table
  (HEARTBEAT, ATTITUDE, GLOBAL_POSITION_INT, RC_CHANNELS, SYS_STATUS
  with per-row sparklines), camera pipeline (codec, resolution, fps,
  bitrate, encoder cpu, restart verb), sensors (IMU/BARO/MAG/GPS chip
  row), plugins (per-plugin state and capabilities).
- **Common dashboard panels.** Cloud relay (mqtt + http state, RTT
  sparkline, masked pairing code with click-to-reveal, Mission
  Control deep-link), network uplink matrix (WiFi AP + client,
  Ethernet, USB tether, 4G modem), services table (cpu, RSS,
  per-row tail-logs, failed-only filter).
- **`GET /api/v1/dashboard/snapshot` endpoint.** Combined 1 Hz
  read-only snapshot of every panel slice. Best-effort: missing
  upstreams render as blank fields rather than failing the request.
- **Two-track polling.** The webapp now runs separate pollers for
  the slow setup status (5 s, backs off to 30 s when hidden) and
  the fast dashboard snapshot (1 s, backs off to 15 s when hidden),
  both wired to dispose on `beforeunload`.

## [0.12.8] - 2026-05-07

### Added

- **One-pager dashboard shell.** The agent's port-8080 webapp is now a
  History-API SPA. A single `index.html` mounts a header, a stat-tile
  row, a panel grid, a bottom dock (mobile only), a settings route, a
  logs route, and a command palette. The visual system ships in a new
  `dashboard.css` with mobile, tablet, and desktop refinements via CSS
  container queries; the five-color status palette is the only thing
  that earns hue.
- **Component vocabulary.** `panel`, `statTile`, `sparkline`, `sheet`,
  `toast`, `contextMenu`, plus helpers `cn`, `clamp`, `debounce`,
  `copyText`, `formatRelative`, `formatRate`. The legacy `el`, `chip`,
  `statusDot`, `liveRow`, `verifyButton`, `streamConsole`,
  `parseMavlinkFrame`, and `decodeMavlinkPayload` carry over unchanged.
- **Keyboard + gestures.** A small key handler binds `?`, `g d / g s
  / g l`, `1-9`, `r`, `j/k`, `p`, and `Esc` on desktop. Mobile gets
  pull-to-refresh, long-press for panel expand, and swipe registration
  hooks.
- **Theme + density.** Dark default, automatic light, opt-in
  high-contrast outdoor mode, persisted in `localStorage`.
- **Accessibility.** Five-color WCAG AA palette, focus-visible rings,
  focus-trap on the command palette and the sheet, ARIA roles on the
  header, dock, palette, sheet, and toasts, `aria-label` on every
  icon-only button, `prefers-reduced-motion` respected.
- **Polling visibility-aware.** Status poll backs off to a slower rate
  when the tab is hidden and disposes cleanly on shutdown.

### Removed

- The eight legacy wizard HTML files (`setup.html`, `mavlink.html`,
  `video.html`, `network.html`, `remote.html`, `ground.html`,
  `system.html`, `advanced.html`). Their content collapses into the
  single SPA shell with section accordions under `/settings`.
- The 1670-line wizard stylesheet `style.css`.

### Changed

- `pyproject.toml` package-data extended to include the new
  `web/setup/components/`, `web/setup/views/`, and
  `web/setup/views/settings/` Python sub-packages so the wheel build
  carries the JS modules.
- Webapp packaging contract test rewritten for the SPA shape.

## [0.12.7] - 2026-05-07

### Added

- **Profile auto-detect always commits a usable value.** The decision
  tail in `ados.bootstrap.profile_detect.detect_profile` is now a
  strict argmax on the live probes, with a stable tiebreaker on the
  last persisted profile and a `drone` default. The legacy
  `unconfigured` outcome that forced first-boot operators through a
  captive-portal wizard is gone. The result includes a new `source`
  field marking which branch of the decision produced the profile
  (`detected` / `tiebreaker` / `override` / `default`).
- **GPS UART probe.** `probe_gps_serial` opens candidate UARTs that
  are not in use by the FC link and looks for an NMEA prefix or a
  UBX sync. A match contributes 3 air points to the score.
- **FC heartbeat probe.** `probe_fc_heartbeat` reads one snapshot
  from `/run/ados/state.sock` and contributes 3 air points when
  `fc_connected` is true.
- **`setup_state` and `profile_source` on the setup status.** The
  REST `GET /api/v1/setup/status` response carries these alongside
  the existing `profile_suggestion` payload so the dashboard banner
  and the cloud heartbeat can show how a profile was picked.

### Changed

- `scripts/install.sh:resolve_profile` no longer accepts the legacy
  `unconfigured` value; a stale write from an older agent falls
  through to the auto-detect step which always returns a usable
  profile.
- `ProfileSuggestion.detected` is now `Literal["drone",
  "ground_station"]`. The agent webapp and the lite-rs setup mock
  no longer reference the legacy third value.

## [0.12.6] - 2026-05-06

Consolidated entry covering 0.10.1 through 0.12.6. The headline themes
since 0.10.0 are: SPI LCD auto-provisioning end-to-end, the lightweight
Rust agent profile shipping in parallel via a separate release channel,
the universal setup webapp moving to a top-level `web.setup` package,
and the install script gaining board-fingerprint auto-detection so a
single curl one-liner installs the right binary on every supported SBC.

### Added

- **SPI LCD auto-provisioning.** Fresh installs detect a supported SPI
  display, install the overlay, and spin up the local dashboard with
  zero follow-up commands. Setup wizard gains a Local display step that
  renders driver-install controls in the universal webapp, persists the
  driver script, pre-selects the matching panel, and exposes a Reboot
  button. The install scripts spawn the overlay-activation helper via
  `systemd-run` to escape the agent sandbox, support u-boot-update for
  Radxa OS Bookworm, and report the attached panel in the heartbeat.
- **Native 480×320 dashboard for SPI LCDs.** Tile router with early-life
  tiles, footer sparklines, and a header that reserves width for the
  BCAST label so it never collides with the clock. Framebuffer renderer
  reads geometry from `virtual_size` + `bits_per_pixel` and scans
  `/sys/class/graphics` for the matching driver.
- **Touch-input bridge for SPI LCDs** wired to the OLED service so the
  dashboard responds to taps without a separate input service.
- **Displays schema on the board profile** (`displays:` block) plus the
  Waveshare 3.5" LCD overlay shipped for Cubie A7Z and Rock 5C.
- **Lightweight backend fields on the board schema** (`libc`,
  `init_system`, `target_rust_triple`, `min_kernel_version`,
  `video.encoder_api_lite`, `video.vendor_lib_loader`,
  `wifi_chip_driver`, `compute.min_ram_mb`) so the lite Rust agent
  reads the same YAML registry as the full agent without a parallel HAL.
- **Pi Zero 2 W board profile** added.
- **RV1106 board profiles** updated to surface `wifi: true` and the
  lightweight encoder API hint.
- **Install script board-fingerprint auto-detect.** `install.sh` reads
  `/proc/device-tree/model` and `/proc/cpuinfo`, fetches the live
  `lite-boards.json` manifest from the lite-agent rolling release, and
  dispatches to `install-lite.sh` for Pi Zero 2 W and Luckfox-class
  boards or continues with the full agent for the rest. New flags:
  `--profile {auto,full,lite}`, `--dry-run`.
- **`--profile` persistence.** The install script remembers the profile
  across upgrades so subsequent runs do not re-prompt or re-detect.
- **Wget-only Buildroot rootfs support** for Luckfox SDK class systems
  that ship without curl. The lite installer falls back from curl to
  wget.
- **Pinned install URLs to release assets** so a curl one-liner always
  resolves to a reproducible artifact instead of a moving HEAD.
- **Setup wizard redesign** with chip vocabulary, two-pane pairing, and
  inline Cloudflare flow. Profile choice and hardware-check steps
  added; profile step folds into a single Continue CTA. The webapp
  rebuilt with shared design tokens. Universal setup webapp relocated
  from `src/ados/webapp/universal/` to a top-level `web.setup` package
  so the lite Rust agent and the Python full agent serve identical
  files via `importlib.resources` and `include_dir!` respectively.
- **Onboarding gating.** The full webapp does not surface until
  onboarding completes.
- **Setup advertised URLs** now point at `/setup.html` and use absolute
  forms so the cloud-relay companion can pick them up directly.
- **CLI:** `ados uninstall` prompts for config purge.
- **Install:** SSH login banner + MOTD now display the setup URL so
  fresh-flashed devices show a clear next step on first login.
- **Network:** ground-station AP passphrase falls back to a known
  default when not yet customized.

### Fixed

- Video pipeline stability: forced constrained-baseline H.264 for WebRTC
  stability, corrected H.264 colour metadata, stopped a wizard
  re-render loop on the video tab, populated the cameras list in
  `/api/video` multi-process branch, fixed an RTSP race during pipeline
  restart, fixed the HAL filter on the wizard preview.
- Video pipeline now pipes `rpicam-vid` through `ffmpeg` for RTSP to
  `mediamtx` so the encoder output stays standard regardless of the
  source binary.
- Install: MOTD source, profile-config parse, and a missing wait for
  the API ready signal that occasionally caused the wizard to land on
  a 404.
- Setup: trimmed the flight-controller step to live chips and a short
  console; set `ArrayBuffer` binary type on the wizard log WebSocket
  so packed frames render correctly.
- Header: reserve width for the BCAST label so it never collides with
  the clock.
- Dashboard: stop early-life tiles overflowing the tile bounds.

### Changed

- Heartbeat now reports the attached display panel alongside the rest
  of the peripheral set.
- Universal setup webapp lives at `web/setup/` (top-level package) so
  both Python and Rust agents serve from the same canonical source.

## [0.10.0] - 2026-05-04

This is a setup-experience overhaul. The agent now owns onboarding for
both drone and ground-station profiles end-to-end, with a single
profile-aware webapp, a four-command public CLI, a setup facade that
clients consume, and a Cloudflare Tunnel quick-install path. The
multi-screen Textual TUI and the broader operator command tree have
been removed in favour of these surfaces.

### Added

- **Setup facade.** New `ados.setup` module assembles a single
  `SetupStatus` document from config, services, network, MAVLink,
  video, and remote-access state. Pydantic models cover `SetupStatus`,
  `SetupStep`, `SetupAccessUrl`, `MavlinkAccess`, `VideoAccess`,
  `RemoteAccessStatus`, `NetworkStatus`, `ServiceState`, and
  `SetupActionResult`.
- **Setup REST endpoints.** `GET /api/v1/setup/status` returns the
  facade payload and is publicly readable on the local node.
  `POST /api/v1/setup/remote-access/cloudflare` accepts a raw
  Cloudflare tunnel token or the install command Cloudflare shows,
  extracts the token, and writes it to a root-owned secret file with
  mode 0600. The token is never echoed back into responses or logs.
- **Universal webapp** at `webapp/universal/`. One static, framework-
  free SPA with a sticky sidebar on desktop, an off-canvas drawer on
  mobile, and nine pages: dashboard, setup, MAVLink, video, network,
  remote access, ground station, system & logs, advanced. The
  dashboard becomes the repeat-visit landing page after onboarding.
  Renders entirely from `/api/v1/setup/status` plus per-page
  helpers.
- **Rich-based terminal status page.** `ados` (no arguments) now opens
  a read-only full-screen status dashboard via Rich `Live` + `Layout`
  when attached to a TTY, and falls back to a concise plain
  summary when run non-interactively. The page surfaces device
  identity, completion percent, the next action, and every advertised
  setup, MAVLink, video, network, and tunnel URL.
- **`config.scripting.mission_control_url`** for operators who run
  Mission Control on a known address. Surfaced through the setup
  facade so the webapp can advertise it.
- **`config.security.setup_token_required`** (default `false`). When
  flipped on, the agent expects an `X-ADOS-Setup-Token` header on
  setup mutations even from same-origin callers. The token is stored
  at `/etc/ados/secrets/setup-token` (0600) and is the strict-mode
  setup-auth posture.
- **Same-origin trust on setup mutations.** The default auth posture
  exempts setup mutations from API-key auth when the request's
  `Origin` header matches the agent's own listening host. Cross-
  origin callers (Mission Control over the cloud relay, anything
  else) still require `X-ADOS-Key`.
- **Host-header validation** in the setup facade. Setup URLs derive
  from a known-good list of local IPs / hostnames / mDNS host /
  hotspot IP / USB gadget IP. Requests with an unknown Host header
  fall back to `localhost:8080` so a hostile upstream cannot inject
  attacker-controlled URLs into setup status.

### Changed

- **CLI surface reduced to four public commands**: `ados`,
  `ados status`, `ados update`, `ados uninstall`. `ados status` adds
  `--json` output for scripting. `ados update` keeps `--check-only`
  and `--yes`. `ados uninstall` keeps `--purge` and `--yes`.
- **Cloud relay payload** carries absolute URLs alongside the legacy
  `lastIp + port` fields: `setupUrl`, `apiUrl`, `videoWhepUrl`,
  `mavlinkWsUrl`. The agent's `missionControlUrl` is now only set
  when an operator configured one explicitly; the legacy mapping
  to the Convex relay URL was removed.
- **Webapp packaging** consolidates to a single root: `webapp/universal/`.
  The legacy `webapp/static/` and `webapp/static-ground/` trees were
  retired and removed. The static mount in `api/server.py` now
  fails loud at startup if the universal directory is missing,
  catching packaging regressions early.
- **`SetupStatus.services`** is now a typed `list[ServiceState]`
  instead of a free-form `list[dict]`.
- **Remote-access config** (`remote_access:`) lifts the Cloudflare
  Tunnel block from optional notes into a first-class config
  section, matching the on-disk shape used by `defaults.yaml`.

### Removed

- **Textual TUI** under `src/ados/tui/`: the nine-screen dashboard,
  every screen module, every widget module, the theme stylesheet,
  the fetcher, and `tests/test_tui_screens.py`. `textual` is no
  longer a runtime dependency.
- **Operator commands**: `ados tui`, `ados gs`, `ados ros`,
  `ados config`, `ados set`, `ados plugin*`, `ados logs`,
  `ados diag`, `ados mavlink`, `ados video`, `ados link`, `ados pair`,
  and the nested `update` subcommands. `ados demo` remains as a
  hidden development entrypoint. Setup, configuration, and
  diagnostics live in the webapp, the API, and Mission Control.
- **Helper modules** that backed the retired CLI surface:
  `cli/_sysinfo.py`, `cli/gs.py`, `cli/help_display.py`, `cli/ros.py`,
  `cli/signing.py`.

### Notes

- This release is an opinionated step away from the older
  multi-tool experience. The four-command CLI is intentional: every
  deeper action moved into the universal webapp, the REST API, or
  Mission Control. Tests in `tests/test_setup_service.py`,
  `tests/test_api.py`, `tests/test_cli.py`, and
  `tests/test_webapp_packaging.py` cover the facade, the auth
  posture, and the webapp packaging contract.
- The companion Mission Control release (v0.9.11) consumes the
  setup facade through a new `getSetupStatus()` agent client method
  and surfaces a Setup-and-access card on Hardware Overview and on
  the disconnected empty state.

## [0.9.8 / 0.9.9] - internal refactors, 2026-05-01 to 2026-05-03

Refactor-only refresh ahead of the universal-setup work. No public
behaviour change. Reflected in monorepo commits 7522981, 7b87131,
c24196d, 65c5893, 59e2c88.

### Changed

- **API runtime facade.** `src/ados/api/runtime.py` decouples REST
  routes from internal agent state. Routes now read through a typed
  facade rather than reaching into the supervisor directly.
- **ServiceTracker module split.** Lifted out of supervisor internals
  into its own module so the setup facade can consume it without
  pulling supervisor scaffolding.
- **Test runtime doubles** consolidated into a shared helper used by
  `tests/test_api.py`, `tests/test_setup_service.py`, and
  `tests/test_cli.py`.
- **Cloud-services rename.** Internal `ados-agent` systemd unit
  renamed to `ados-supervisor` to match the supervisor module's role
  and to free `ados-agent` for the public CLI.
- **Discovery shutdown.** `src/ados/services/discovery.py` awaits the
  zeroconf unregister task before closing, fixing a race that left
  stray mDNS records on a fast restart.
- **Ground-station pairing CLI restructure.** Internal-only; pairing
  primitives moved out of the public CLI surface ahead of the
  4-command consolidation.

### Added

- `AGENTS.md` with agentic-coding instructions for AI contributors.

## [0.9.7] - 2026-04-30

### Added

- IPC dispatch capability gate. The plugin-runtime IPC server now
  decorates each method with the capability it requires. Calls from a
  plugin whose token does not carry the capability are rejected with
  `capability_denied: <cap>` before the handler is reached. Eight
  telemetry, mission, recording, and MAVLink stub methods are gated
  ahead of their handler implementations so the contract stays
  enforceable as those subsystems land. The Python plugin client maps
  the wire error back to a `CapabilityDenied` exception.
- Capability lookup helpers on `ados.plugins.capabilities`:
  `get_granted_caps`, `has_capability`, `require_capability`. Each
  consults the supervisor's install record so the same authoritative
  source backs both the runtime gate and operator-facing tooling.
- `PluginTestHarness` SDK at `ados.sdk.testing`. Plugin authors get an
  in-process `PluginContext` wired to a fake IPC client, capability
  injection, captured publishes, and YAML scenario replay. Manifest
  field `agent.test_fixtures` maps friendly names to fixture paths the
  harness resolves at replay time. Path traversal is rejected at
  manifest validation.
- `ados plugin test <plugin_dir>` subcommand. Validates the plugin
  manifest, exports `ADOS_PLUGIN_*` env vars, and shells out to
  `pytest` against the plugin's `tests/` directory so authors can run
  their suites against the harness with a single command.
- `tmpfiles.d` rule sweeps stale `/run/ados/plugins/*.sock` entries on
  boot. Hard-killed plugin processes used to leave socket inodes
  behind that blocked the next `bind()`; the rule lets the supervisor
  rely on a clean socket directory.

## [0.9.6] - 2026-04-30

### Added

- Two new built-in plugins shipped under `ados.plugins.builtin`.
  `telemetry-logger` subscribes to the public lifecycle topics and
  emits a structured log line per event for journald and operator
  dashboards. `mavlink-inspector` subscribes to vehicle state changes,
  folds them into an in-memory snapshot, and republishes the snapshot
  under its own plugin namespace for diagnostic UIs. Both run inprocess
  under the first-party signer carve-out and serve as worked examples
  for the SDK contract.
- Canonical capability catalog at `ados.plugins.capabilities`.
  Enumerates the 29 named agent permissions a plugin manifest may
  declare. Manifest validation now logs a warning when it sees a
  capability outside the catalog. The catalog is advisory; runtime
  enforcement gates land per surface as the protected subsystem ships.
- Plugin OEM deployment guide at `docs/oem/plugin-deployment.md`.
  Covers signed-archive distribution, signing key registration,
  factory-time install, key revocation rotation, CLI quick reference,
  resource limits, and troubleshooting.
- `tmpfiles.d` rule for `/run/ados/plugins` socket runtime cleanup,
  installed automatically by the install script.
- `--yes` / `-y` flag on `ados plugin perms --revoke` for non-interactive
  use. Default is to prompt before revoking a granted permission.
- IPC capability token expiry is now enforced per request inside the
  supervisor's IPC dispatch loop. Expired tokens return a structured
  `token_expired` error envelope and the request is not routed.

### Changed

- `scripts/install.sh` now provisions the `ados-plugins.slice` cgroup
  parent and the plugin runtime tmpfiles drop-in idempotently during
  install and upgrade. Fresh-flashed SBCs no longer need any manual
  steps to host plugins.
- Three internal-tag comments in `pyproject.toml` rewritten as neutral
  domain comments describing what the configuration does.
- Dev dependencies extended with `msgpack` and `python-multipart` so
  the IPC, RPC, and API plugin test files collect under `pytest`.

## [0.9.5] - 2026-04-30

### Added

- `ados plugin lint` subcommand. Runs static analysis on a `.adosplug`
  archive (banned Python and JavaScript patterns, network imports
  versus declared permissions, vendor-binary flag, signature presence).
  Returns a scored report and exits non-zero on errors. Same rule set
  the registry submission gate will run server-side.

## [0.9.4] - 2026-04-30

### Added

- Driver-layer base classes for hardware-driver plugins under
  `ados.sdk.drivers`. Covers camera, gimbal, LiDAR, GPS, ESC, and
  payload actuator. Each base ships an abstract class plus frozen
  dataclasses for candidates, capabilities, and per-stream value types.
- Driver error hierarchy (`DriverError`, `DriverDeviceNotFound`,
  `DriverPermissionDenied`) chained under the existing plugin error
  base so driver faults flow through the supervisor's circuit breaker.
- Top-level `ados.sdk` package re-exporting the public driver surface
  for plugin authors.
- Contract tests for the driver base classes covering abstract-ness,
  trivial-subclass instantiability, frozen value types, and error
  hierarchy.
