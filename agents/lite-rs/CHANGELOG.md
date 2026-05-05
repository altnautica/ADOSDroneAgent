# Changelog — `ados-agent-lite`

All notable changes to the lite agent are documented here. The lite agent
versions independently of the Python full agent at `src/ados/`.

Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)

## [Unreleased]

### Added
- Centralized `atomic_write` helper at `crates/ados-setup/src/atomic.rs`
  with O_CREAT|O_EXCL + mode-at-create + fsync + rename + parent dir
  sync semantics. Wired into 5 callsites (state.json, pairing.json,
  agent.yaml mutations, cloudflared token, cloud profile choice).
- `sysmetrics` module reports CPU / memory / SoC temperature on each
  heartbeat tick. Reads `/proc` via the `sysinfo` crate plus
  `/sys/class/thermal/thermal_zone0/temp`. Best-effort — missing
  thermal zone returns None instead of failing the heartbeat.
- `AgentMeta` carries static board metadata (board name, soc, arch,
  ram_mb) plus network identity (hostname, last_ip, mdns_host) on the
  heartbeat body. Populated once at startup via
  `ados_setup::hardware::detect_board_metadata`. The GCS fleet card
  reads these fields directly without re-fingerprinting.
- `cloudflared` install verifies SHA256 against the upstream-published
  `<asset>.sha256` companion file. `ADOS_CLOUDFLARED_SKIP_SHA256=1`
  bypasses for offline test environments.
- WebSocket cloudflared log streamer caps sessions at 15 minutes so a
  forgotten browser tab cannot pin a `journalctl` child indefinitely.
- `Update` and `Uninstall` CLI subcommands matching the universal
  four-command operator contract.
- `pair <CODE>` CLI subcommand writes pairing.json directly (no agent
  restart needed; the cloud client re-reads pairing state on every
  beacon).
- `scripts/verify-webapp-sync.sh` hashes the canonical `web/setup/`
  against the embedded copy at
  `agents/lite-rs/crates/ados-setup/web-setup/` and prints a sync
  command on drift. Wired into CI.
- OEM-facing key-rotation runbook at
  `docs/oem/lite-agent-key-rotation.md`.

### Changed
- `CloudConfig` no longer carries `api_key` directly. The cloud client
  reads pairing state from `/etc/ados/pairing.json` on every iteration
  so `ados-agent-lite pair CODE` from another process flips beacon →
  heartbeat without an agent restart. Legacy agent.yaml configs with
  `cloud.api_key` are migrated into pairing.json on first boot.
- MQTT subscriptions are now routed: inbound `mavlink/rx` frames are
  forwarded to the FC writer queue; `command` and `webrtc/offer`
  publishes are logged at INFO. Previously all inbound traffic was
  dropped silently.
- MQTT `clean_session` flipped to `false` so unsent frames survive a
  reconnect.

### Fixed
- Pairing flow was structurally non-functional pre-rebuild: empty
  `pair_code` in cloud beacons, conflated `pair_code` with `api_key`
  on disk. Rebuilt as a proper PairingStore with a 900-second TTL on
  generated codes (matching the Python full agent's PairingManager).
- Five bench-day install-script edge cases shipped: curl→wget fallback
  for Buildroot images, lite-v* tag fallback when no stable release
  exists yet, busybox-tar gzip-pipe extraction, /usr/local/bin
  pre-creation on minimal rootfs, minisign placeholder-key auto-skip.
- Webapp absolute-path drift between the Python full agent's
  root-mount and the Rust lite agent's `/setup/`-mount produced a
  blank screen on Luckfox. Fixed with a root-mount fallback in the
  Rust router; CI parity check guards future drift.

### Test
- 88 tests across 4 crates (ados-mavlink 6, ados-cloud 6, ados-setup
  unit 56, ados-setup integration 16, ados-agent-lite 4). All green.

## [0.1.0] — 2026-05-05 — Phase 1 control plane

First validated release. Control plane only — no video, no WFB-ng. The
binary runs end-to-end on a Luckfox Pico Zero (Rockchip RV1106G3,
256 MB DDR3L, ARMv7 uclibc, busybox sysv-rc) and a Pi Zero 2 W
(Broadcom BCM2710A1, 512 MB, aarch64 glibc systemd) via cross-compile.

- `crates/ados-mavlink` — MAVLink v2 parser + serial port owner +
  broadcast fanout to local consumers.
- `crates/ados-cloud` — MQTT-over-TLS publisher (rumqttc + rustls),
  HTTPS heartbeat + pairing beacon, exponential backoff with 5-minute
  ceiling.
- `crates/ados-setup` — full universal setup REST surface (11 axum
  handlers + 1 WebSocket), board fingerprint engine, Cloudflare Tunnel
  orchestration, embedded webapp via `include_dir!`.
- `crates/ados-agent-lite` — main binary, `clap` CLI with `serve`,
  `pair`, `update`, `uninstall`, `version`, demo-mode subcommand.
- `proto/` directory carries the language-neutral contracts
  (cloud OpenAPI, MQTT topic schema, setup-api.yaml, RTSP path
  conventions, IPC framing, CLI commands) — the lite agent and the
  Python full agent both conform.
- `web/setup/` is the canonical webapp location served by both
  backends; the lite agent embeds the same files at compile time.
- CI workflow at `.github/workflows/lite-agent-release.yml` produces
  signed `lite-v*` tagged releases plus a rolling `lite-agent-main`
  artifact set on every push to `main`. Build matrix:
  `armv7-unknown-linux-musleabihf`, `aarch64-unknown-linux-{musl,gnu}`,
  `x86_64-unknown-linux-musl`. Each artifact is minisign Ed25519
  signed against the first-party release-artifact key.
- `scripts/install.sh` auto-detects the board via
  `/proc/device-tree/model` + `/proc/cpuinfo` + `/proc/meminfo`,
  dispatches to the lite path on lite-eligible boards, and supports
  `--profile {lite,full,auto}`, `--dry-run`, `--pair PAIRCODE`,
  `--upgrade`, and `--uninstall`.
