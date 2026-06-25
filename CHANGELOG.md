# Changelog

All notable changes to the ADOS Drone Agent are recorded here.
Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
the project follows [Semantic Versioning](https://semver.org/).

## [0.98.1] - 2026-06-26

### Added

- An inert `atlas` config gate (`atlas.enabled`, default off) for the
  world-model feature. Mirrors the `vision` gate. The capture and compute
  services read it when they ship; nothing runs while it is off.

## [0.62.0] - 2026-06-11

### Added

- Per-service memory on `/api/services` is now backed by the durable logging
  store. The supervisor samples each service's PSS continuously (the same
  cgroup-grouped sum the on-demand scan does) and ships it to the store, so the
  route reads the latest value from history first and falls back to the live
  `/proc` scan on any gap. Values are identical to the live scan; the live scan
  stays the default and the fallback.

## [0.61.0] - 2026-06-11

### Added

- The ground-station mesh reads (`/mesh`, `/mesh/neighbors`, `/mesh/routes`,
  `/mesh/gateways`), the relay and receiver status reads, and the aggregate
  `/network` active-uplink leg plus the `/network/modem` cumulative-usage block
  are now backed by the durable logging store: each reads the store first and
  falls back to the live read unchanged, never a 500. The mesh, relay, receiver,
  and uplink/data-cap writers ship their snapshot bodies to the store on every
  write, so these reads keep serving when the live in-process source is degraded
  or briefly unavailable. The live read stays the default and the fallback;
  response shapes are byte-identical. The `/role`, `/mesh/config`, live Wi-Fi
  scan/status, and configuration reads stay served from their live sources.

## [0.60.0] - 2026-06-11

### Added

- The radio status and history reads (`/api/wfb`, `/api/wfb/history`,
  `/api/wfb/pair/failover-status`) and the video air-pipeline and latency reads
  (`/api/v1/video/air-pipeline`, `/api/video/latency`) are now backed by the
  durable logging store: each reads the store first and falls back to the live
  read unchanged, never a 500. On a native-radio rig `/api/wfb/history` returns
  durable link history (previously empty on that path), and the reads keep
  serving when the live source is briefly unavailable. The live read stays the
  default and the fallback; response shapes are byte-identical.

## [0.59.0] - 2026-06-11

### Changed

- The agent reaches the cloud backend only when configured for an explicit
  cloud posture. The status heartbeat, the command poll, and the pairing
  beacon emit to Convex only when `server.mode` is `cloud` or `self_hosted`;
  an absent, `local`, or unrecognized mode stays silent rather than beaconing,
  so a local-first or mistyped configuration never reaches out to the cloud.
  Local pairing over the LAN is unaffected.

### Fixed

- A ground station restores its receive adapter to a NetworkManager-managed
  state when the receive plane stops, complementing the restore done before a
  bind, so a shutdown never leaves the adapter in monitor mode for the next
  bind to trip over.

### Removed

- The superseded in-process Python ground-station cloud-relay bridge. The
  native cross-profile cloud relay owns the uplink-reactive reconnect, the
  data-cap downshift, and the status heartbeat on both profiles.

## [0.58.0] - 2026-06-11

### Fixed

- The management-link reach-back no longer flaps into WiFi-heartbeat mode on a
  rig managed over onboard WiFi with an unplugged Ethernet port. A wired
  interface is treated as a management primary only once it has carried a link,
  so a port unplugged from boot is ignored while a real cable pull still fails
  over. The WFB injection adapter is also classified as wireless by interface
  name, so a transiently-unreadable sysfs entry during monitor-mode bring-up can
  never misclassify it as a wired primary.
- A drone in local mode keeps retrying the local WFB bind instead of giving up
  after a fixed number of attempts. The cloud-relay fallback now fires only when
  a cloud relay is configured (`server.mode` is `cloud` or `self_hosted`), so a
  local-first rig never strands itself with no link.
- The WFB injection adapter is restored to a NetworkManager-managed state before
  the bind receiver starts, so a ground-station bind no longer aborts when the
  adapter was left in monitor mode by the receive plane.

### Changed

- The on-box agent CLI authenticates over loopback on a paired agent. A request
  from the host's own loopback interface, not relayed by a proxy or tunnel, is
  trusted, so commands like `ados radio status` work on the box without the
  root-owned pairing key.

## [0.51.0] - 2026-06-04

### Changed

- The closed-loop FEC controller is now armed by default. On a link with
  received-side statistics it raises the Reed-Solomon ratio under packet loss
  or weak signal and lowers it again on a sustained clean window. A drone with
  no peer statistics yet holds its current rung (the cold-start guard), so the
  default is inert until a ground station is in range. Pin a manual rate and
  redundancy from Mission Control or the agent webapp to turn it off.
- The radio status now reports the data plane's live FEC ratio and MCS index
  (what the transmitter is actually sending after a tuning change or an
  automatic step), rather than the values from the configuration file.

### Added

- A link preset (conservative / balanced / aggressive) can now be applied at
  runtime over the radio command socket, not only at boot. The preset sets the
  base rate and redundancy; an armed adaptive controller keeps adjusting from
  there.
- Radio tuning changes (FEC ratio, MCS index, link preset, adaptive on/off) are
  now written back to the configuration file so they survive a service restart,
  matching how transmit power was already persisted.

## [0.50.34] - 2026-06-04

### Fixed

- Ground stations no longer restart-loop the camera-encode pipeline. The
  encode pipeline is air-side only (a ground station receives video through its
  own media relay) and its binary is fetched on the drone profile only, but the
  supervisor was not profile-gating it, so a ground station that had previously
  run as a drone kept starting a unit whose binary was correctly absent. The
  unit is now drone-gated and torn down on a ground-station install.
- The logging and telemetry store no longer wedges in a restart loop on a large
  store. Its startup readiness was gated behind a full structural check whose
  cost scales with the store size; on a multi-hundred-megabyte store that ran
  past the unit's start timeout before the daemon could signal readiness. The
  boot path now uses a fast structural check, with added start-timeout headroom,
  so a large store starts cleanly; the full deep check remains for deliberate
  audits.

## [0.50.33] - 2026-06-04

### Added

- USB-rehome self-heal: when a radio adapter is on a slow USB port AND its RF
  is unverified (transmitting but no confirmed reception), both held across a
  confirm window, the agent unbinds and rebinds the USB device for a clean
  re-enumeration that can land it on a faster lane, then re-checks. Bounded to a
  small attempt budget with an escalating cooldown and a terminal exhausted
  state (never a reset loop). A fail-closed guard refuses any rehome that could
  touch the operator's management link. The state rides the heartbeat
  (`usbRehomeState`) so Mission Control shows the recovery. Default-on under
  `network.usb_rehome`.

## [0.50.32] - 2026-06-04

### Added

- Onboard-WiFi heartbeat reach-back: when the wired primary management link is
  physically down for a sustained window and an onboard WiFi has a usable path,
  the agent declares a heartbeat-only fallback so the box stays visible to
  Mission Control (status push and command receive only — video and full
  telemetry stay on the primary and resume when it returns). Hysteresis on both
  edges so a brief cable blip never triggers it. The mode rides the heartbeat
  (`mgmtLinkMode`) and Mission Control renders the degraded reach-back posture
  distinctly. Default-on under `network.mgmt_failover`.

## [0.50.31] - 2026-06-04

### Added

- Management-link guardian: the supervisor now watches the operator's
  management link (the default-route interface, never the radio injection
  adapter) for a dead data path — no carrier, no routable lease, or an
  unreachable gateway — and walks a bounded, self-restoring software repair
  ladder without a reboot: re-assert the regulatory domain, renew DHCP,
  reconnect Wi-Fi, bounce the interface, restart the network backend. Works
  across NetworkManager and systemd-networkd, climbing one rung per check so a
  bounce of the operator's own link self-restores. The link state and repair
  progress ride the heartbeat (`managementLink`) so Mission Control shows a
  degraded-but-up link distinctly from a healthy one. Default-on under
  `network.management_link_guardian`.

## [0.50.30] - 2026-06-04

### Fixed

- The installer now provisions the `ados` system user and group during the
  systemd step. Previously the plugin runtime directory and plugin service
  units referenced `ados:ados` and the ground-station hardware-group setup ran
  `usermod -aG <grp> ados`, but nothing created the account — so the runtime
  directory could not be owned correctly and the group membership step quietly
  did nothing. Provisioning is idempotent and the account is a no-login system
  user.

## [0.50.29] - 2026-06-04

### Changed

- Trimmed a companion-computer chip-comparison note from the published `docs/`
  tree. It was hardware-selection R&D planning material (market price estimates,
  per-chip viability calls) rather than operator or developer documentation.

## [0.50.28] - 2026-06-04

### Changed

- Trimmed two development planning notes (a bench-test log and per-board memory
  estimates) from the published `docs/` tree — R&D planning material, not
  operator or developer documentation — and removed a stale cross-reference to
  them from the chip-comparison note.

## [0.50.27] - 2026-06-04

### Removed

- Repository cleanup of stale trees that no installer, package, CI, or runtime
  path referenced: the `buildroot/` documentation stub (its backend is not
  built), the `configs/` example YAMLs (the authoritative defaults live in
  `src/ados/core/defaults.yaml`), the `mockups/lcd-ui/` design mockups and
  `assets/lcd-icons/` SVGs (the LCD UI is native and draws its icons
  procedurally), the `scripts/render-lcd-icons.py` SVG-to-PNG helper, and its
  `icon-tools` packaging extra. Trimmed the obsolete Blockly section from the
  samples README. No functional change.

## [0.50.26] - 2026-06-04

### Fixed

- `ados logs push` now works without `sudo`. The push request is recorded under
  the root-owned runtime directory, so a non-root operator hands the write to
  the running agent over its local API (loopback, same-origin) instead of
  failing with a permission error. Root still records the request directly, and
  a clear message is shown when the agent is unreachable and the caller is not
  root. No new system groups or directory permissions are introduced.

## [0.49.59] - 2026-06-01

### Added

- The degraded-USB-adapter signal now rides the cloud heartbeat and the drone
  status block, so the ground station can warn about a radio adapter on a slow
  USB link over the cloud relay, not only on the local link.

## [0.49.58] - 2026-06-01

### Added

- **The radio detects and reports a degraded USB adapter.** It reads the WFB
  adapter's enumerated USB link speed and flags it when an adapter comes up on a
  slow (full-speed, 12 Mbps) USB link instead of high-speed — a state where the
  driver loads and the transmit counter advances but no usable RF leaves the
  antenna. The selected adapter's speed and a degraded flag are logged loudly at
  selection and carried on the heartbeat + the adapters sidecar so the ground
  station can warn instead of showing a healthy link.

## [0.49.57] - 2026-06-01

### Fixed

- **Installs survive a flaky network.** Binary downloads now resume from where
  they dropped instead of restarting from zero (`curl --continue-at -`), and
  each binary is retried with exponential backoff, so a management link that
  drops mid-download no longer aborts the whole install. The per-fetch ceiling
  is raised to 180s.

## [0.49.56] - 2026-06-01

### Documentation

- Add a release runbook describing how the rolling native binaries and the
  versioned wheel + bundle are published, and why the release tag is created
  by hand.

## [0.49.55] - 2026-06-01

### Removed

- **The packaged Python WFB transmit and ground direct-receive planes.** With
  the native radio link proven over the air, the drone transmit plane and the
  ground direct-role receive plane now run their native binaries only — there is
  no Python fallback, and a missing or broken binary fails loud. `ados rust
  status` no longer lists the radio or ground-receive services (there is nothing
  to switch), and the operator radio knobs always route to the native command
  socket. The mesh relay and receiver roles keep their packaged module for now.

## [0.49.54] - 2026-06-01

### Changed

- Re-locked the dependency set after the ground-station display moved to the
  native renderer, so the imaging packages it no longer needs are dropped from
  the lock file. Added a `vision` install extra that carries numpy for the
  on-device inference sidecars, kept out of the default and drone installs.

## [0.49.53] - 2026-06-01

### Fixed

- **Ground-station displays keep their overlay across installs.** A
  ground-station install now provisions its SPI display overlay instead of
  stripping it, so the panel keeps its framebuffer through an upgrade. Drone
  installs still revert the overlay so the GPU keeps the framebuffer.

## [0.49.52] - 2026-06-01

### Fixed

- **Cheap USB cameras stop wedging on boot.** The installer disables USB
  autosuspend on the kernel command line, so a camera that mishandles the
  autosuspend resume no longer drops off the bus before the video service comes
  up. The video service also restarts without a start-limit cap if a camera
  wedges repeatedly.

## [0.49.51] - 2026-06-01

### Fixed

- **Video reaches the radio reliably.** The video relay's first start waits
  until the camera stream is ready and respawns from the run loop if it exits,
  so the transmitter always has video to send. Operator transmit-power changes
  route to the native radio's command socket.

## [0.49.50] - 2026-06-01

### Added

- Ground receive answers hop announcements and drives its self-heal watchdog
  from live decodes, so a healthy link is not torn down during a quiet beacon
  window.
- Operator FEC, modulation, transmit-power, and tier controls reach the native
  transmit plane directly.
- The heartbeat reports the radio stack state, the failover state, and the
  transmit-side restart counters.

## [0.49.49] - 2026-06-01

### Fixed

- **Ground video reaches the radio again.** The drone-side video relay now
  respawns when its feed process exits, and the first relay start waits until the
  camera stream is ready, so the radio always has video to transmit.

### Changed

- **Radio adapter selection reads the USB vendor and product IDs reliably.** The
  selector resolves the adapter's USB device node up the device tree, so the
  vendor/product table and the management-interface exclusions apply on real
  hardware. A malformed regulatory domain is rejected before it reaches the radio.
- **Channel changes are verified.** A channel set is confirmed against the live
  interface before the link is reported as moved, and a stuck command times out
  rather than stalling the link.
- **Auto-pair is more careful.** It validates the stored key before treating the
  link as paired, and skips a bind attempt (without spending a retry) when no
  radio adapter is present.
- **Touchscreen calibration uses the shared transform.** The ground-station LCD
  touch input loads a saved calibration when present and otherwise falls back to
  an orientation-correct default.

## [0.49.48] - 2026-06-01

### Added

- **Runtime-mode native reporting.** The radio and ground-station link services
  report whether they are running the native binary or the packaged fallback, so
  the active runtime mode is visible end to end rather than inferred.

### Fixed

- **Installer binary placement is atomic and cache-resistant.** Prebuilt service
  binaries are fetched fresh (the fetch defeats stale intermediary caches) and
  written by rename rather than in place, so a partial download can never replace
  a working binary. The global command symlinks match the on-disk binaries.
- **Cloud status maps the radio adapter and pairing fields correctly.** The
  ground control station reads the radio adapter and pairing state from the
  cloud status payload using the current field layout, so the radio panel shows
  the real link state.
- A lint finding in the service layer was corrected.

## [0.49.26] - 2026-05-29

### Changed

- **The native cloud relay and video orchestrator are now the only
  implementations.** Both passed on-rig validation, so their service units exec
  the native binaries unconditionally and the installer always provisions them.
  The standalone Python cloud-relay service and the Python video service entry
  point have been removed. The reusable video library modules (pipeline,
  encoder, mediamtx manager, camera manager, local tap, SEI tools) and the cloud
  MQTT MAVLink relay and heartbeat helpers stay, since the in-process demo
  pipeline and the ground-station services still use them. Cloud relay continues
  to default to local mode; it is the secondary, opt-in remote path.

### Added

- **Native ground-station binaries.** The ground-station data plane, uplink
  matrix, human-interface arbiter and input reader, and display writer ship as
  prebuilt binaries. Each ground-station service selects the native binary when
  it is present and the matching opt-in flag is set, falling back to the
  packaged Python service otherwise.

### Fixed

- **Native uplink cutover no longer leaves the per-interface managers running.**
  When the native uplink daemon owns the link, the packaged ethernet, wifi
  client, and USB-gadget managers are now disabled (their start link removed)
  rather than masked, which silently failed on units that ship as real files;
  hostapd and the modem service stay available because the native daemon drives
  them. A manager slow to stop is reset so it does not linger in a failed state.
- **Native services can persist state.** The uplink, cloud, and supervisor
  service units now include the agent state directory in their writable paths,
  so the data-cap counter, command-idempotency records, and the setup-complete
  marker write correctly under the strict filesystem sandbox.

## [0.49.0] - 2026-05-29

### Removed

- **ROS 2 integration.** The opt-in ROS 2 environment has been removed: the
  Docker container, the MAVLink bridge, the workspace and recording managers,
  the `/api/ros/*` routes, the `ros.environment` capability flag, the `ros`
  config section, the `ados-ros` service, the dashboard ROS page, and the
  per-board ROS capability flags are all gone. The agent no longer manages a
  robotics environment; integrations that need one can be built on the plugin
  system instead.

## [0.44.0] - 2026-05-27

### Fixed

- **Radio link now comes up out of the box (rendezvous, then hop).** A drone
  and ground station could pair but never establish the WFB link: the drone
  auto-scanned onto 5 GHz U-NII-1 channels (36-48) that the ground adapter's
  regulatory domain disables, so the two sides settled on different channels
  and never met, while the drone's adapter sat in managed mode transmitting
  nothing. Both sides now start on a fixed home channel (149, U-NII-3, enabled
  under essentially every regulatory domain) and bring the link up there;
  coordinated channel hopping only activates after the link is established,
  is constrained to channels the local adapter actually enables, and falls
  back to the home channel if a hop loses the peer.

### Changed

- Default radio band is U-NII-3 and bind-time channel auto-relocation is off,
  so the drone and ground deterministically rendezvous on the home channel.
- The drone verifies its interface actually entered monitor mode before it
  starts transmitting, and transmits on the home channel as soon as its key is
  present rather than waiting to hear the ground first.
- New optional `video.wfb.reg_domain` config applies a wifi regulatory domain
  on both sides at radio bring-up so they enable the same channel set. Unset
  by default; the home channel works without it.

## [0.43.9] - 2026-05-27

### Fixed

- **Wi-Fi driver no longer fails to build from a compiler crash.** On some
  toolchains gcc segfaults (internal compiler error) while optimizing one of
  the driver's source files at `-O2`, which aborts the whole module build and
  leaves a board with no radio. The driver now builds at `-O1` (for both the
  on-device build and the prebuilt pipeline), which compiles cleanly with no
  measurable runtime cost for a NIC driver. The dkms.conf patch is now
  content-aware, so a changed compiler-flag set actually re-applies instead of
  being skipped because an older flag set was already present.
- **A failed radio driver build is now reported honestly.** The install
  recorded the radio as present whenever a leftover module-source marker file
  existed, even if the current build had failed and loaded no module. It now
  confirms the module is actually loaded or resolvable before claiming success,
  clears a stale marker otherwise, and records a degraded radio so the fleet
  view and install result reflect reality.

## [0.43.8] - 2026-05-27

### Added

- **Prebuilt Wi-Fi driver fast-path.** The radio-driver install now tries a
  verified prebuilt kernel module matched to the exact running kernel before
  compiling on the device, so a board on a published kernel skips the slow
  (and, on marginal hardware, crash-prone) on-device build entirely. Any miss
  (no manifest reachable, no module for this kernel, vermagic mismatch, failed
  verification, or failed load) falls through to the existing DKMS build, so
  behavior is unchanged until a driver manifest is published. SHA256 integrity
  is always enforced; signatures are verified when a key is configured
  (`ADOS_DRIVER_PUBKEY`) and skipped in dev (`ADOS_PREBUILT_ALLOW_UNSIGNED`).
  Skippable with `ADOS_DRIVER_PREBUILT=0`.
- **Driver build + publish pipeline.** A CI workflow builds the patched
  RTL8812EU module against the stock Raspberry Pi OS kernels (Pi 3 / 4 / 5 /
  Zero 2 W) inside distro-matched containers, records each module's SHA256
  (and an optional minisign signature when a key is configured), and publishes
  the modules plus a `drivers-manifest.json` as release assets that the
  fast-path above consumes. Vendor-BSP kernels keep the on-device build path.
  The fetch plus SHA256 plus vermagic-match chain is hardware-validated; a
  kernel with no matching module falls back to the on-device build.

## [0.43.7] - 2026-05-27

### Fixed

- **Install recreates a virtual environment it cannot repair.** If the
  build toolchain still cannot be staged after clearing corrupt leftovers
  (a pip damaged badly enough to crash even on a wheel install), the venv
  is rebuilt from scratch. The fresh venv ships the interpreter-bundled pip
  (older than the regressed line, with working build isolation) and clean
  packages; config and pairing live outside the venv and the agent is
  reinstalled right after, so nothing is lost. The pip pin now only pulls a
  broken newer pip back rather than upgrading a known-good bundled one.

## [0.43.6] - 2026-05-27

### Fixed

- **Install recovers from a corrupt virtual environment.** An install or
  upgrade that was interrupted mid-build can leave a half-written
  distribution behind (pip names it `~name`); the next pip run then crashes
  reading its metadata, which aborted the agent-software step on an
  already-touched venv. The build-toolchain step now clears those corrupt
  leftovers before it installs, so a venv damaged by an earlier failed run
  heals itself on the next install.

## [0.43.5] - 2026-05-27

### Fixed

- **The agent install no longer dies on a broken system pip.** A recent
  pip release crashes (SIGSEGV) the moment it starts an isolated build
  environment on some arm64 kernels, which killed the agent-software step
  outright on the edge channel. The install now pins pip below that line
  and stages the build backend (setuptools, wheel) into the venv via plain
  wheel installs (which never use build isolation, so they succeed even on
  the broken pip), then builds the agent with normal isolation on the
  pinned pip. This also self-heals a venv whose pip was already updated to
  the broken version.
- **Wi-Fi driver fast path checks the exact installed source.** It now
  looks at the precise versioned DKMS source tree instead of globbing, so
  a board carrying more than one build version cannot match the wrong one,
  and the build-confinement step is skipped cleanly when the CPU mask is
  not accepted.
- **Display provisioning fails closed on every board.** No boot-config
  file is edited (Allwinner extlinux/env, Pi config.txt, Rockchip
  extlinux/managed.list, Armbian armbianEnv) unless a restorable snapshot
  was saved first; if a snapshot cannot be written the overlay is skipped
  so the board still boots. Probation is only armed when a snapshot exists.

### Changed

- **Pruned dead code and pre-alpha compatibility shims** from the install
  scripts: unused helpers and variables, a retired config-host migration,
  legacy profile-file and single-service migrations, and the duplicated
  profile-name alias handling (the profile name is normalized once at the
  entry point). No behavior change.

### Added

- **Install smoke test.** `scripts/test/install-smoke.sh` shellchecks every
  install script (including the modular `install.d/` set, which CI lint
  previously skipped), syntax-checks them, and probes `--dry-run` profile
  resolution. Wired into CI so this class of breakage is caught before it
  reaches a board.

## [0.43.4] - 2026-05-27

### Fixed

- **Silenced the spurious Wi-Fi driver warning on monitor-mode teardown.**
  The radio driver maps the adapter's role (AP / mesh / station / adhoc)
  to a disconnect action and warns on anything else. A monitor-mode
  interface, which is how the radio link runs, has none of those roles,
  so every interface-down logged a kernel warning even though the cleanup
  that follows is harmless. A source patch
  (`data/driver-patches/monitor-disconnect-warn.patch`) adds an explicit
  monitor / no-link case so the path stays quiet. The driver build also
  rebuilds correctly when only the source patches change: the install
  now verifies the on-disk DKMS source carries the current patch before
  taking the already-built fast path, and clears the copied source tree
  before re-adding so a freshly patched build is never skipped.

## [0.43.3] - 2026-05-27

### Fixed

- **Drone installs no longer provision an on-board status panel.** The
  install defaulted the display to `auto` on every profile, so a drone
  with a panel physically attached would apply an SPI-LCD overlay, edit
  the boot config, and cost an extra reboot to light up a panel that
  nothing draws to: the on-panel dashboard renderer runs on the
  ground-station profile only. The default is now profile-aware. The
  ground station auto-detects and provisions whatever panel is present;
  the drone and lite profiles default to `none` with no detection and no
  boot-config writes. An operator who wants a panel on a drone can still
  force it with `ADOS_DISPLAY=<id>`.

## [0.43.2] - 2026-05-27

### Fixed

- **Wi-Fi driver build is confined by CPU affinity so it cannot knock
  the board offline.** Setting the DKMS `parallel_jobs` hint alone was
  not enough: some DKMS versions pick their `make -j` from the core
  count and ignore `framework.conf`, so the compile still ran one job
  per core and starved the USB Wi-Fi management link until the board
  went unreachable mid-build. The build is now pinned to two cores with
  `taskset` (affinity is inherited by every gcc it spawns), leaving the
  remaining cores free for the kernel's USB and network work. The
  `parallel_jobs` hint and renice are kept for DKMS versions that honor
  them; both degrade gracefully when the tool or knob is absent.

## [0.43.1] - 2026-05-27

### Fixed

- **First attempt to keep the Wi-Fi driver build from starving the
  network link.** Set the DKMS `parallel_jobs` hint to two and reniced
  the build. Superseded by 0.43.2 after on-hardware testing showed the
  DKMS version in use ignores `parallel_jobs` and still compiles one job
  per core; a CPU-affinity cap was needed instead.

## [0.43.0] - 2026-05-26

### Added

- **Displays auto-configure by physical presence.** The installer now
  probes for a connected panel and provisions it without operator input:
  a bound SPI-LCD framebuffer is recognized as-is, an HDMI output is used
  when a connector reports connected, and an I2C OLED is enabled when it
  answers on the bus. A declared-but-unbound SPI-LCD is applied on
  probation and confirmed at next boot by `ados-display-probe`, which
  restores the previous boot config automatically if the panel fails to
  bind. When nothing is attached the display resolves to `none` and no
  boot config is touched. The on-screen UI service is gated on a single
  `/etc/ados/display.enabled` marker rather than a loose framebuffer
  glob.
- **Staged install progress.** The foreground install prints numbered
  stage banners with elapsed time and emits periodic heartbeats during
  the long steps (driver compile, dependency install) so a headless
  operator can see it is still working.

### Changed

- **Services skip cleanly when their hardware is absent.** The OLED,
  button, and modem services exit without error instead of retrying when
  their device is not present, so an install on a board without that
  peripheral leaves no failed units behind.
- **The Wi-Fi driver is built from source via DKMS only.** The agent
  trusts the on-disk DKMS module rather than a shipped binary, and the
  heartbeat reports the module source as `dkms`.

## [0.39.0] - 2026-05-25

### Added

- **Per-stream video transmit watchdog.** The WFB manager now watches the
  video transmitter's UDP ingress backlog independently of the shared
  radio byte counter. A healthy video stream drains its socket
  continuously; when the transmitter wedges (process alive but no frames
  leaving the radio) the backlog pins at the kernel buffer ceiling while
  the encoder keeps pushing. The watchdog detects that within ~15 s and
  restarts the pipeline so video recovers on its own, even while the
  control plane keeps the shared interface counter moving. The heartbeat
  now carries `tx_video_stalled`, the stall recovery count, and the
  current ingress backlog so Mission Control can surface a stalled
  transmitter remotely.

### Fixed

- **Ground-station mesh service no longer flaps on direct-role nodes.** On
  a node in `direct` role the mesh manager now exits cleanly instead of
  reporting the intentional no-op as a failure, which had made systemd
  restart-loop the unit until it landed in a failed state.
- **Rockchip ISP daemon quieted on USB-camera rigs.** Boards that ship the
  Rockchip `rkaiq_3A` ISP service but capture from a USB camera no longer
  carry it in a failed state. The installer masks it only when it is
  present and not already running, so a board genuinely using a MIPI
  camera keeps it. Reversible with `systemctl unmask rkaiq_3A`.

## [0.38.1] - 2026-05-24

### Fixed

- **Ground-station downlink video over the local (LAN-direct) path.** The
  consolidated `/api/status/full` video block now uses the same WHEP probe
  as `/api/video`, so a ground station reports its received downlink as
  running over the direct connection, not only via the cloud relay.
- **Receive-link metrics over the local path.** `/api/status/full` now
  carries a camelCase `radio` block (RSSI/SNR/noise/loss/MCS/FEC plus
  receive-liveness), so Mission Control surfaces the link metrics when
  connected directly to the agent.

## [0.38.0] - 2026-05-24

### Added

- **Ground stations re-stream received video to the cloud.** A ground
  station decodes the drone's H.264 over the radio and republishes it on
  the local WHEP endpoint. The heartbeat now advertises `videoState` and
  `videoWhepPort` for the ground-station profile so Mission Control plays
  the received downlink through the same path it uses for a drone camera.
  The stream is advertised only when frames are actually arriving
  (`/api/wfb` reports `connected` with a positive packet count), not on
  process-liveness alone.
- **Richer receive-link metrics in the heartbeat radio block.** Added
  `snr_db`, `noise_dbm`, `loss_percent`, `mcs_index`, and
  `rx_silent_seconds` (receive-liveness) alongside the existing RSSI /
  bitrate / FEC fields, on both transmit and receive sides. The ground
  station's `/api/wfb` view now also persists `rx_silent_seconds`.

### Fixed

- **Log entries now carry an ISO-8601 string timestamp.** The in-memory
  log buffer was emitting a raw float epoch, which broke clients that
  treat the timestamp as a string. Both the REST endpoint and the live
  log stream now return an ISO-8601 string.

## [0.28.12] - 2026-05-16

### Added

- **Navigation wizard: VIO camera orientation field.**
  `POST /setup/navigation/config` accepts a new optional
  `vio_camera_orientation` field (`forward`, `downward`, `auto`).
  Operators flying over ground (agriculture, survey, SAR, pipeline
  patrol) pick `downward`; operators flying indoor / corridor /
  inspection pick `forward`. The wizard rejects `forward` or
  `downward` on optical-flow modes (which are always downward) and
  rejects `downward` when no downward camera is discovered.
- **Navigation wizard: firmware field.** `POST /setup/navigation/config`
  accepts `firmware: "ardupilot" | "px4" | "inav"`. Betaflight is
  intentionally absent and gets rejected by Pydantic with a 422.
  iNav + VIO modes get rejected at validation time because iNav's
  external position-injection EKF integration is not VIO-grade.
- **Wizard-to-plugin translation step.** `translate_wizard_to_plugin_config()`
  converts the wizard's simplified 4-mode + orientation + firmware
  vocabulary into the plugin's 6-mode + camera-orientation schema
  when persisting `config.yaml` under `/etc/ados/plugins/<id>/`.
  Operators never see the plugin's native mode names; the wizard
  speaks `optical-flow` / `vio` / `both` and the plugin reads
  `optical_flow` / `vio_vins_fusion` / `hybrid_of_plus_vio`.
- **HAL board profile: `cameras:` block.** Additive optional metadata
  on every board profile YAML. Each entry carries `name`, `bus`,
  `orientation`, and `notes`. The vision-nav wizard reads this to
  default the camera-orientation picker. Rock 5C Lite profile populated
  with `front=forward` and `down=downward` entries for the dev rig.
- 10 new tests on `tests/api/test_setup_navigation.py` covering the
  new orientation + firmware fields, the wizard-to-plugin translation,
  iNav-VIO rejection, and Betaflight schema rejection. Total nav
  route test count goes from 17 to 27.

## [0.28.10] - 2026-05-16

### Added

- **Plugin SDK fill: real `PluginContext`.** The Python `ADOSPlugin`
  base class and `PluginContext` now ship as real implementations
  rather than spec stubs. Plugins receive a context object that
  exposes `ctx.events.publish / subscribe`, `ctx.mavlink.send` and
  `ctx.mavlink.subscribe`, `ctx.peripheral_manager.register_camera /
  register_depth_sensor`, `ctx.config.get / set / on_change`,
  `ctx.agent_id`, and `ctx.process.spawn`. Each context method
  enforces the plugin's declared capability grants at call time.
- **`subprocess_spawn` allowlist.** Manifest schema v2 adds an
  explicit allowlist of vendor binaries a plugin may exec. The
  supervisor enforces the allowlist at spawn time via a new
  `process_sandbox.py` that inherits the plugin's cgroup limits, pipes
  stdio, and rejects any path not in the manifest. This is the
  sandbox guarantee for plugins that ship pre-compiled binaries.
- **`vendor_attribution` manifest field.** Required when
  `contains_vendor_binary: true`. Carries `upstream_repo`,
  `commit_sha`, `license`, and `source_offer_url` so the install
  dialog can surface GPL §6 source-offer compliance details to the
  operator before installation.
- **Three new agent capabilities.** `mavlink.component.vio` (HIGH
  risk) registers MAVLink component ids 197 and 198 on the vehicle
  bus. `estimator.pose.inject` (CRITICAL risk) authorizes submission
  of `ODOMETRY`, `VISION_POSITION_ESTIMATE`, `VISION_POSITION_DELTA`,
  and `VICON_POSITION_ESTIMATE` to the FC's state estimator. Both are
  catalogued in `ados.plugins.capabilities` and gated by the IPC
  dispatcher.
- **`OPTICAL_FLOW_RAD` MAVLink encoder.** Plugins with the
  `mavlink.component.vio` capability can now emit `OPTICAL_FLOW_RAD`
  (msg id 106) through `ctx.mavlink.send`. The encoder lives at
  `src/ados/protocol/mavlink/encoders/optical_flow.py` and registers
  CRC_EXTRA for clean parser round-trips.
- **`SET_GPS_GLOBAL_ORIGIN` and `MAV_CMD_SET_EKF_SOURCE_SET`
  encoders.** Both are required for GPS-denied flight setup. The
  agent's pre-arm helper dispatches `SET_GPS_GLOBAL_ORIGIN` when the
  EKF reports "waiting for home" and a plugin has registered itself
  with the vision component id.
- **HAL board YAMLs gain navigation fields.** Every board profile under
  `src/ados/hal/boards/*.yaml` adds `navigation: { optical_flow,
  vio }` where each value is `none`, `cpu_only`, or `npu_accelerated`.
  Plugin installers refuse to install on boards whose declared
  navigation tier doesn't cover the plugin's needs. The vision-nav
  plugin requires `optical_flow >= cpu_only` and `vio >=
  npu_accelerated`.
- **Setup webapp `/setup/navigation/*` routes.** Three new routes on
  the universal setup webapp under `web/setup/views/navigation/`
  preview the camera enumeration result, the rangefinder bus
  availability, and the FC firmware detected. These are read-only
  diagnostics; per-drone vision-nav config still happens through
  Mission Control's plugin configuration drawer.
- **`RemoteInstallReceiver` and LAN-direct install.** The agent
  accepts plugin install commands through two transports: the
  existing `cmd_droneCommands` cloud-relay queue (for the HTTPS GCS
  case) and a new `/api/v1/plugins/install` LAN-direct endpoint (for
  the local-network HTTP GCS case). Both transports converge on the
  same supervisor pipeline; both honor the same signature and trust
  list. The LAN-direct path is gated by the WS auth ticket flow.

### Changed

- **MAVLink router registers `MAV_COMP_ID_VISUAL_INERTIAL_ODOMETRY`
  (197) and the optical-flow companion convention (198).** Plugins
  with `mavlink.component.vio` claim one of those component ids on
  install and emit traffic under that component on the vehicle bus.

### Security

- **WS auth ticket on the plugin LAN-direct install endpoint.** The
  endpoint previously accepted unauthenticated install commands when
  the GCS was on the same LAN. It now requires a short-lived ticket
  minted by the GCS through the existing pairing handshake, scoped to
  the install operation, and bound to the requesting origin. Tickets
  expire after 60 seconds.
- **Signed-URL allowlist on the plugin downloader.** The agent's
  `.adosplug` downloader now allowlists Convex storage origins and
  the configured registry origin. Downloading from arbitrary URLs
  requires an operator override flag on the `ados plugin install`
  CLI, which the GCS never invokes.

## [0.13.3] - 2026-05-07

### Added

- **Heartbeat carries setup_state + profile_source.** The cloud
  heartbeat payload now includes `setupState` (always
  `"configured"` for a live agent) and `profileSource`
  (`"detected"`, `"tiebreaker"`, `"override"`, `"default"`, or
  `"user"`). Mission Control reads these to render an
  "auto-configured" pill on drone cards whose profile was picked
  by the boot-time detect rather than the operator.

## [0.13.2] - 2026-05-07

### Added

- **Live profile switch with auto-restart.** `apply_profile()` accepts
  an optional `auto_restart=True`. When the profile actually changed,
  the agent dispatches `systemctl --no-block restart
  ados-supervisor.service` (D-Bus first, subprocess fallback) so the
  new profile's services come up without an SSH follow-up. The
  response surfaces `auto_restart_attempted`, `auto_restart_ok`, and
  `auto_restart_message` fields under the section's `data`.
- **Reconnect sheet on profile change.** When the settings sheet
  receives a successful apply with a profile-restart attempted, the
  webapp opens a non-dismissable sheet that polls
  `/api/v1/setup/status` at 2 s intervals for up to 60 s, waits for
  the new profile to appear, then routes back to the dashboard. A
  "go to dashboard now" escape hatch is always available. If the
  agent does not return in time the sheet surfaces an error toast.
- The settings profile section now sets `auto_restart: true` on its
  apply payload by default.

## [0.13.1] - 2026-05-07

### Added

- **Batch settings apply.** New `POST /api/v1/setup/apply` accepts a
  combined delta (profile, network, cloud, display, advanced) and
  runs each section's setter inside a single try/rollback block.
  Per-section results return as a structured `ApplyResponse` so the
  UI can show partial-success cleanly. Rollback restores the live
  config slice in reverse order when a later section fails.
- **Network and advanced section setters.** New
  `src/ados/setup/network.py` writes WiFi SSID + password +
  hotspot toggle onto `runtime.config.network`. New
  `src/ados/setup/advanced.py` validates log level + board override
  + factory-reset flag. Each setter handles a None payload as a
  no-op success so the apply route can iterate without
  special-casing absent sections.
- **Settings sheet form controls.** Each of the five sections at
  `web/setup/views/settings/{profile,cloud,network,display,advanced}.js`
  renders real form controls bound to a per-section dirty tracker.
  The Apply button label updates reactively as `apply (n changes)`,
  posts ONCE to `/api/v1/setup/apply`, and toasts per-section
  results. Cancel resets every tracker.

## [0.13.0] - 2026-05-07

### Added

- **Ground-profile dashboard panels.** WFB-RX (adapter, channel,
  frequency, per-stream RSSI chips, packet loss, FEC stats, RSSI
  sparkline), mesh status (role badge, batman-adv peer table with
  link quality and last-seen, gateway node, partition state),
  stream sources (aggregated bitrate sparkline, per-source FEC and
  dedup stats), local display (device, kiosk URL, refresh rate,
  current content), OLED + buttons (current screen, brightness,
  per-button mapping, last button event), joystick (HID identity,
  axis bars, button chips).
- **Role-based panel composition.** The dashboard view selects the
  ground panel set by `ground_role`: `direct` shows wfb_rx +
  display + oled_buttons + joystick; `relay` adds mesh; `receiver`
  adds mesh + sources. The view rebuilds when role flips, not just
  when profile flips.
- **Snapshot extension.** `/api/v1/dashboard/snapshot` now carries
  seven new keys (wfb_rx, mesh, sources, display, oled, buttons,
  joystick) alongside the eight Phase C keys. Helpers fall back to
  config-derived defaults when a runtime summary method is missing.

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
