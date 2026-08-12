# Changelog

All notable changes to the ADOS Drone Agent are recorded here.
Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
the project follows [Semantic Versioning](https://semver.org/).

## [0.99.358] - 2026-08-12

### Removed

- **Four superseded Python subsystems, about 7,600 lines.** Each had been
  replaced by a native service some time ago and stayed on disk, shipping in
  every wheel and read by anyone tracing the code.

  The packaged plugin host: nothing constructed it, and the native host had
  owned the per-plugin sockets since the default flipped. Its fallback marker
  goes with it, because a marker selecting a deleted implementation is worse
  than none — the unit resolved its `ExecStart` to `/bin/true` when the marker
  was present, so a box pinned to the packaged path had no plugin host at all.
  The marker is now pruned on upgrade, which is what recovers such a box.

  The single-process cloud runtime: the pairing beacon, status heartbeat and
  command poll sat behind a config flag defaulting false that nothing set. The
  native cloud service has served all three throughout, so this was a second
  implementation that only ran if someone opted in by hand.

  The packaged button service: the native arbiter has read the front-panel GPIO
  in-process since the PIC cutover, and the installer stopped and disabled the
  packaged unit on every run.

  The packaged video pipeline: the in-process GStreamer pipeline, the local tap
  and the recorder. The native video service has run the encode path alone since
  its unit gained a binary-presence condition, and the only thing still naming
  the packaged pipeline was a branch that no systemd unit could reach. What the
  native service genuinely uses stays: it shells out to the SEI injector and the
  headless tap by module name.

- **The air-side pipeline read chain, which had no writer.** Nothing wrote
  `/run/ados/air-pipeline.json` once the in-process GStreamer pipeline went, but
  the entire read side outlived it: a route on both transports, a ten-metric
  series and a state event in the logging store, a store-to-route reconstructor,
  two conformance specs, and a merge block in `GET /api/video` that no surviving
  pipeline could trigger. The route answered 204 forever and the series had
  nothing to sample.

  Removed with it: a router module that declared no routes at all, so its mount
  was a no-op; two helpers left with no non-test caller; and a subprocess slot in
  the native video service that is initialised to empty and never filled, whose
  teardown could only ever do nothing.

- **The pipeline-buffer latency reading, which could only ever be empty.** It came
  from a GStreamer latency query on a pipeline object inside the Python tap. The
  drone-side tap now reads the local RTSP feed through ffmpeg, and a subprocess
  cannot be asked for its internal pipeline latency, so the surviving writer had
  been hardcoding the field to null while the route, the metric series and the
  GCS popover row all still carried it. The number is not reproducible under the
  current architecture, so it is gone rather than replaced by a different
  measurement wearing the old label.

- **The `use_gst_air_pipeline` toggle.** It selected between two air-side
  pipelines, one of which no longer exists. Nothing read it, yet its default ran
  an SoC detect and a file read on every config load in every Python service, and
  it was still offered to operators in the settings schema.

### Fixed

- **Glass-to-glass latency samples were being dropped, more on some encoders than
  others.** The Annex-B parser computed each NAL's end by assuming the four-byte
  start code. Behind the three-byte form — which `h264parse` emits depending on
  upstream caps — that lands one byte early, and the trailing-zero trim then
  removes a real payload byte. The SEI arrives short, fails its own length check,
  and the marker vanishes.

  A fix for exactly this had been made once and was lost when two same-named
  parsers were consolidated onto the copy that never received it. Measured across
  60,000 timestamps: 236 samples lost against 1. The loss is value-dependent, so
  it reads as intermittent, and on an encoder that always emits the three-byte
  form it is every frame.

- **A button-mapping change from the GCS took effect only after a restart.**
  Both reload paths signalled `ados-buttons.service`, which the installer always
  tore down, while the daemon that actually owns the mapping and rebuilds it on
  SIGHUP was never signalled at all. The write returned success and the running
  mapping stayed as it was.

  Found while deleting that unit, by tracing who signalled it. The reload now
  goes to the arbiter, and a refused signal is logged rather than swallowed, so
  a save that lands on disk without reaching the running process leaves
  something to explain why.

- **A malformed `config.yaml` read as an unconfigured feature.** Four route
  modules each carried their own reader that collapsed a parse failure into the
  same empty object they return for an absent file. They now share the loader
  that keeps the missing case quiet and logs the parser error — naming the
  offending field — when the file is present but unreadable.

- **The headless profile answered 404 for features it simply does not carry.**
  The permanent-Python prefix table listed features by name rather than by mount
  point, so three live ones never matched and a request for them read as "no
  such path" instead of "this build does not carry that feature".

- **A failed TX-power save reported success silently.** The atomic write
  collapsed every I/O fault into a bool that the caller discarded, so a setting
  that reverted on the next restart had nothing in the log. The radio still
  accepts the value, so the response is unchanged; what changes is that the
  durability failure is now attributable.

## [0.99.355] - 2026-08-03

### Fixed

- **HLS is remuxed only while something is watching it.** `hlsAlwaysRemux` kept
  a low-latency muxer segmenting the stream at one second whether or not any
  HLS client existed.

  Measured on a ground station while an operator's video was freezing: the muxer
  sat attached to the main path as a third reader with `bytesSent=0` — it had
  never delivered a single byte to anyone — while the box ran at load 4.94 on
  four cores and two WebRTC sessions competed with the on-screen browser.

  HLS stays available; the on-box dashboard's video panel uses it, and mediamtx
  now starts the muxer on the first request instead of holding one open forever.
  The cost is a slightly slower first HLS frame, paid by whoever asks for it.

## [0.99.354] - 2026-08-03

### Fixed

- **The access point finds its radio by driver, not by name.** Interface names
  are not stable: measured across three reboots of a ground station, `wlan0` was
  the onboard Broadcom chip twice and the USB long-range radio once. A hardcoded
  `wlan0` therefore meant a one-in-three chance of configuring the access point
  on the aircraft's radio link.

  Resolution reads the same generated adapter tables the radio itself consults,
  so the two cannot disagree about which adapter is which. Two independent
  guards, because the cost of being wrong is taking the aircraft's link down:
  resolution avoids the radio by driver, and the config write refuses outright
  if the interface it ended up with is the one the radio reports it opened. A
  box with no onboard WiFi gets no access point rather than stealing the radio.

- `network.hotspot.interface` is now a real setting. A status route already
  reported it while nothing consumed it, so a value an operator set was shown
  back to them and then ignored.

## [0.99.353] - 2026-08-03

### Fixed

- **The HDMI cockpit no longer comes up black.** The kiosk unit exported an
  `XDG_RUNTIME_DIR` that logind only creates for a real login session. A system
  service never gets one, so on a freshly installed appliance the directory was
  simply absent and the compositor failed at startup with "Unable to open
  Wayland socket".

  The compositor process stays alive after that failure, so the unit read
  `active running` while the screen showed nothing but the framebuffer cursor.
  It only ever appeared to work on boxes where a desktop session had already
  created the directory. The kiosk now points at a path the agent creates and
  owns.

## [0.99.352] - 2026-08-03

### Fixed

- **Every unit generates its own access-point passphrase.** Two paths still put
  the same passphrase on every unit: the entropy-failure branch substituted a
  single compiled-in string while its own comment claimed to be failing closed,
  and — more damagingly — that same string was the shipped configuration
  default. A configured passphrase takes precedence over a generated one, so
  per-unit generation was skipped on any box that read the default.

  The default is now empty, which means "generate one". Losing the access point
  when the random number generator fails is recoverable and visible; a network
  that presents as protected while sharing one published key across every unit
  is neither. The generated value is shown on the display page, the on-box
  console and the installer's completion card.

- **A factory reset erases identity as well as credentials.** The shell script
  erased the device identity, configuration and logs while the API path
  preserved them, so the two disagreed about what a factory reset meant. They
  now share one list: a reset unit comes back indistinguishable from a freshly
  flashed one. The profile marker is still preserved — it records what the
  hardware is rather than who holds it.

## [0.99.351] - 2026-08-03

### Fixed

- **The hardware watchdog is off by default, and now arms last.** It turned a
  slow startup into a board that would not boot.

  The step arms a timer that HARD-RESETS the SoC — no shutdown, no log, no trace
  — when PID 1 is late by more than a few seconds. It ran BEFORE the step that
  starts every service. On a ground station whose service startup exceeded the
  timeout, the board reset itself mid-install and mid-write, then did the same on
  the next boot, and the one after. Each reset landed during a write, so the card
  degraded, so boot got slower, so the reset came sooner. It ends in a card that
  will not boot at all.

  This is the same failure this tree already removed once by another mechanism.
  `kernel.hung_task_panic` was turned off because "a hung task is nearly always
  slow hardware, and the box is still correct — it needs to be allowed to finish,
  not shot." The hardware watchdog was doing exactly that by a different route,
  and was left armed.

  Three changes: the default is off (`network.watchdog.enabled: true` opts in);
  an unreadable config resolves to off rather than open, because failing open
  arms a hard-reset timer on precisely the box whose config could not be read;
  and an upgrade now REMOVES an existing drop-in even on a board where the device
  check would have skipped, so a rig already carrying it is actively reverted
  rather than left looping.

  The step also moved after `health`. It is default-off now, but someone will opt
  in, and arming a reset-on-stall before the step most likely to stall is wrong
  on its own merits.

  The capability is kept rather than deleted, and that is deliberate: a genuinely
  frozen kernel in the field cannot be recovered by any software, and the SoC
  watchdog brings the box back with nobody driving out to it. That is a field
  property. On a bench, where a human can power-cycle, it has cost more outages
  than it has prevented.

## [0.99.350] - 2026-08-03

### Fixed

- **One aircraft could occupy two fleet slots.** A device id is
  `uuid4().hex[:12]`, and an 8-character form of the same id is derived from it
  for naming. The slot registry compared ids as exact strings, so the two forms
  of one drone were two drones: it took a second slot, and the hero fan-out
  promoted and demoted the same airframe in a single call.

  Slot lookup now recognises the short form as the aircraft it was derived from.
  The rule is deliberately narrow — the exact 8-from-12 hex derivation, both
  sides hex, nothing else. A general "one starts with the other" rule was written
  first and was wrong: it merged `drone-1` into `drone-11`, which an existing
  fleet-full test caught on the first run. Identifiers that merely share a
  leading substring are not the same aircraft, and a rule loose enough to merge
  them would eventually bind a command to the wrong airframe — worse than the
  duplicate slot it fixes.

  An ambiguous short form, where two registered ids share it, resolves to nothing
  rather than to whichever was found first. An empty id matches nothing; absent
  is not a wildcard.

## [0.99.349] - 2026-08-03

### Fixed

- **A node with the logging store switched off carried a permanently failed
  unit.** Both rigs showed `ados-logd.service` failed after the store shipped off
  by default, on every boot, forever.

  The daemon does the right thing: it reads the toggle, logs that it is
  declining, and exits 0. But the unit is `Type=notify`, so systemd waits for a
  readiness notification and records a process that exits without ever sending
  one as `result 'protocol'` — a failure — however clean the exit code. The
  installer's `mask` could not paper over it either, because the unit file is a
  real file at the exact path `systemctl mask` needs for its symlink, so masking
  silently did nothing.

  The disabled path now announces readiness and then exits, which is the correct
  handshake for "started successfully, and there is nothing to do": systemd
  records a clean start and a clean stop, and `Restart=on-failure` has no failure
  to act on.

  Worth stating why this was not cosmetic. A node that always shows a failed unit
  teaches its operator to stop reading failed units, and the next one that
  matters gets read the same way. The whole point of switching the store off was
  to stop the box lying about its own health.

## [0.99.348] - 2026-08-03

### Fixed

- **The Link tab crashed the moment it was opened.** "Objects are not valid as a
  React child", with the offending keys named in the error itself:
  `{chipset, driver, supports_monitor}`.

  The agent sends `adapter` as an object. The screen had it hand-typed as a
  string and rendered it directly, so the type and the wire disagreed and
  nothing existed to notice. The whole tab went down, not just that row -- an
  error thrown during render takes its subtree with it.

  The field is now typed as it actually is and read through a helper that
  returns a string or nothing, whatever the wire sends. Blank fields (a ground
  station whose radio has not been probed reports empty strings for all of them)
  render as absent rather than as a stray separator.

  The same endpoint carries two other object-valued fields, `aux_lane` and
  `enabled_channels`. Neither is rendered anywhere, so nothing else of this shape
  is waiting to fire.

## [0.99.347] - 2026-08-03

### Fixed

- **The cockpit showed telemetry and a black video frame from any browser other
  than the panel's own.** Closing the LAN video hole in 0.99.328 routed the media
  plane through the auth edge, and the edge refused the operator's dashboard-PIN
  session for video while accepting it for everything else. On-box requests
  returned 200 and off-box returned 403, so the ground station's own screen
  played and a laptop did not -- which is exactly the shape that hides a
  regression.

  Two session validators exist, because an unpaired node has no pairing key to
  key its HMAC with and mints sessions under a different issuer. The edge called
  the paired-only one, which returns false whenever the node is unpaired. A
  dispatcher that picks the right one per pairing state already existed, and its
  own comment described this divergence as "harmless only by accident: an
  unpaired node's data plane was open anyway, so the stricter one was never
  consulted". Gating the media plane made it consulted. The edge now calls the
  dispatcher.

  The hole stays closed. A caller with no PIN and no key still gets nothing.

### Added

- **The media plane accepts the session as a query parameter.** A `<video>`
  element issues its own requests for a playlist and its segments and offers no
  hook to attach a header, so a header-only credential is unreachable for
  element-driven playback -- the operator gets a black frame with no way to
  authenticate it.

  Confined to `/whep` and `/hls`. A credential in a URL lands in access logs,
  browser history and `Referer`, so it is not accepted anywhere on `/api/*`,
  where every caller is code that can set a header. An empty value reads as
  absent rather than as a credential, and the parameter name is matched exactly.

- **The battery warning requires a measured voltage.** A flight controller with
  no battery monitor reports 0% at 0.0V, and the banner accepted that as an empty
  pack, so every bench session without a battery raised a red "Battery low - 0%
  remaining" over the video. A real pack at 0% still has voltage. A critical
  alarm that fires every session is one an operator learns to dismiss, and it
  gets dismissed just as fast on the flight where it is true.

## [0.99.346] - 2026-08-03

### Added

- **Release one drone's fleet slot without dropping the fleet.** The only reset
  a ground station offered was station-wide: it wipes the radio keys and drops
  every member. That is the wrong tool for "this airframe is being retired or
  re-flashed", and with nothing better available a bench removed a drone by
  editing the registry file by hand -- a runtime patch of the kind that leaves a
  box in a state no install can reproduce.

  `DELETE /api/v1/ground-station/wfb/pair/:device_id` frees one slot. The
  registry already had the operation; nothing exposed it.

  It does not touch keys. A fleet shares one radio keypair, so the released
  drone keeps working until it is re-paired or re-flashed. What is freed is the
  slot number, so the next drone to pair takes it rather than the station
  reporting itself full while holding registrations for airframes that no longer
  exist. Releasing a device that holds no slot reports it absent rather than
  succeeding, because a typo that reads as a completed release is a typo nobody
  goes looking for.

## [0.99.345] - 2026-08-03

### Fixed

- **A rig holding a radio key from a peer that no longer exists had no way
  back.** Auto-pair decided "am I paired?" by looking at the shape of the key
  file — 64 bytes, hashable second half — and disarmed permanently when the
  answer was yes. A key left behind by a ground station that was since reflashed
  passes that check perfectly.

  Two rigs sat unlinked for a whole session on exactly that. One held a
  structurally valid key from a peer that no longer existed and injected into a
  void, its own radio reporting `rf_unverified`, which means transmitting with
  zero confirmed reception. The other had been reflashed, had no key at all, and
  blocked. Neither attempted recovery. Every surface reported health.

  The signal that means "this key does not work" was already being computed, and
  already being surfaced by `ados diag link`. Nothing consumed it. Now something
  does: auto-pair may re-arm itself for a key whose fingerprint has never once
  been confirmed to work, and never for one that has.

  That single rule is what makes this safe. A stale key from a reflashed peer
  re-arms, because the peer that would have proven its fingerprint is gone. A
  pair that has worked is latched for life, so an outage of any length — a
  minute, a week — is structurally incapable of re-opening a bind window on it,
  which matters far more than the recovery does. Silently re-binding a working
  pair would be a worse failure than the deadlock being fixed. And a successful
  re-bind writes a new key whose different fingerprint resets the record, so the
  new key starts with a clean lifetime rather than inheriting the old one's.

  The two rigs need different evidence and neither transfers. A drone reads
  `rf_unverified` rather than an absent channel lock, because `rf_unverified`
  requires the transmitter to be live — so an idle radio, or one that never
  started, accumulates nothing. A ground station never measures a transmit path
  at all, so its equivalent is `searching`: key present, receive chain running,
  nothing decoding. Pointedly not the blocked states, which mean the chain never
  ran and there is therefore no verdict on the key to act on.

  Bounded so that recovery cannot become a bind storm: a ten-minute confirm hold
  that restarts in full on any release, five episodes per key ever, and a
  half-hour cooldown between them. The budget and the cooldown live in
  `/var/lib/ados/wfb-pair-proof.json` rather than under `/run`, because on tmpfs
  a reboot would erase both and a rig in a boot loop would reintroduce the storm
  the budget exists to bound. A stale radio sidecar resets the hold rather than
  freezing it; a bind already running suspends the trigger, since a bind window
  is `rf_unverified` by construction and would otherwise feed itself. All of it
  is tunable under `video.wfb.pair_rearm`, and on by default — the deadlock is
  silent and permanent, so it must not need enabling.

- **An unpaired ground station published nothing at all.** The pairing gate sits
  at the very top of the receive loop, ahead of adapter resolution, and returned
  to the top without writing a sidecar. Not a degraded reading — the absence of
  one: no state, no interface, no reason. That is why the failure above took a
  live session to find, with one half of an unlinked pair missing from every
  surface except the journal.

  It now writes a `blocked_unpaired` sidecar alongside the existing reg-blocked
  and no-injection ones. Nothing has been examined at that point in the loop, so
  every hardware verdict is null rather than a confident boolean about an adapter
  the gate never looked at; the one thing the body asserts is why the plane is
  deaf. Refreshed every twenty seconds rather than on all twelve polls a minute:
  the gate still polls at five so a key landing is picked up promptly, but the
  file only has to stay inside a reader's staleness window, and this is a flash
  card.

### Added

- **`PUT /api/wfb/pair/auto-pair` takes an optional `force`.** Re-arming a rig
  that already holds a key was refused outright, which left exactly one route:
  unpair first, which deletes the key. If the key was in fact fine — a peer
  merely off, a radio merely down — that turns "possibly stale" into "definitely
  gone" and makes things strictly worse while trying to diagnose them.

  A forced re-arm records a one-shot against the key's own fingerprint and leaves
  the key exactly where it is. If the bind that follows fails, the rig still has
  what it had. Keying it to the fingerprint is what keeps it honest: a key
  replaced between the request and the supervisor's next tick no longer matches,
  so the request is discarded rather than firing at whatever key is there now.
  The field defaults off, so a client that does not send it sees the refusal
  exactly as before.


## [0.99.344] - 2026-08-03

### Changed

- **The janitor is now bounded by how much space the agent occupies, not by how
  full the card happens to be.** Free-space percentage was the wrong signal and
  would have caught none of the failures this work exists to prevent: a 128 GB
  card at 3% used can carry a store growing by a gigabyte a day, and no
  percentage threshold fires until the day there is nothing left to trim
  gracefully. Occupied space is what breaks these nodes — the card fills, a
  rewrite cannot get its scratch, a write tears, the filesystem corrupts and the
  box will not boot. That sequence caused the reflashes; wear did not.

  There is now a total footprint budget, 5 GB by default, split into per-category
  caps that sum to it: the logging store, quarantined copies of a torn store,
  recordings, plugin logs, the audit trail, the journal, and apt. Each cap is
  enforced at every rung, so a category over its share is trimmed even on a box
  with room to spare — recordings cannot quietly take the store's allowance on a
  node that happens not to be logging. Within a category, oldest goes first.
  Free-space percentage is kept as the secondary net for a card the agent shares
  with something else, and the harsher of the two signals picks the rung.

  `/opt/ados` — the venv, the runtime, the models and the binaries, 605 MB on a
  drone — is measured and reported but never reclaimed, and deliberately sits
  **outside** the budget. It is the installed product rather than accumulation:
  it does not grow while the box runs, deleting any of it breaks the agent
  rather than freeing space, and counting it inside would mean a release
  shipping a bigger model silently ate the allowance for recordings.

  The floors are unchanged and still outrank the caps. A single quarantined
  store larger than the entire quarantine share is not a hypothetical — it is
  what the drone was holding — so it survives, and the residue is reported as a
  category the janitor declined to fix rather than quietly accepted.

- **`ados diag storage` now leads with the footprint.** Total against budget,
  then each category against its own cap with any excess named, then the
  installed agent marked as not counted. The write rate is still reported,
  because wear is still real, but it follows rather than leads. A box whose
  janitor has not measured yet says so instead of printing a total of zero,
  which would claim the agent occupies nothing at all.

## [0.99.343] - 2026-08-03

### Changed

- **The cockpit no longer blurs its chrome over live video by default.** The
  page draws its chrome over full-bleed video behind a backdrop blur, and a
  blurred region above a surface that changes every frame makes the compositor
  re-read and re-blur its backdrop at the video's frame rate. The cockpit's own
  source already named it the most expensive thing the page does; nobody had
  switched it off on this board.

  Measured on a ground station with video actually arriving over the radio,
  which is the first time this was measured under real conditions rather than on
  an idle page: 292.9% of 400% CPU with the full layer against 137.9% with the
  reduced one, and board idle rising from 36% to 68%. Same video, same frame
  rate, 53% less CPU.

  It had been gated on the board having under 3 GiB of RAM, which is the wrong
  quantity — the blur costs compositor time, not memory. A 3.8 GiB four-core
  panel sat above that threshold, paid full price for an effect invisible behind
  a HUD, and was the board the reduced path helps most. Reduced is now the
  default and an operator who wants the blur asks for it; the RAM heuristic is
  deleted rather than left as a second unreachable route to the same flag.

- **The durable logging and telemetry store now ships OFF by default. This is a
  deliberate capability regression, not an optimisation.** While it is off the
  node has no durable flight recorder: nothing survives a reboot except the
  journal, and the journal does not survive a reflash. That is the cost, stated
  plainly, and it is the reason the persistent journal is being kept rather than
  volatilised.

  What it buys: measured on a drone, the node wrote 904 KB/s with the store
  running and 49 KB/s with it stopped. The store is roughly 96% of everything
  reaching the card and the largest single lump of space it occupies. Cards were
  filling, tearing and being reflashed largely because of it. It is a real
  feature and it will come back, but it currently costs more than it returns.

  One key controls it: `logging.store.enabled`, default false, in the config
  schema so it renders in the settings UI. Off means the unit is disabled and
  masked and the daemon declines to start, so no store file is created — the
  installer decides from the key, and the daemon reads it again so a unit
  started by hand or left enabled by an older install still declines. Turning it
  back on is that one key plus a re-run of the installer, never a reinstall; the
  binary is placed either way. The legacy pin from `ados rust disable logd` is
  still honoured as a force-off, so a box an operator turned off by hand is not
  quietly turned back on by an upgrade.

  The gate defaults to off when the config is absent, malformed, or predates the
  key — the opposite direction from every other gate in the agent, and on
  purpose. The others default their feature on because a config a box cannot
  read must not silently disable a safety net. This one is not a safety net: a
  typo that turned it on would hand the node back the write volume that has been
  destroying cards, and losing history is recoverable with one key where a
  reflash is not.

### Fixed

- **Everything that reads the store now degrades honestly rather than reporting
  a fault.** With the store off, "could not connect" is the ordinary case on
  most nodes.
  - `ados logs` says the store is off, that this is the default, how to reach
    live logs through `journalctl`, and how to turn it back on. It no longer
    asks whether `ados-logd` is running, and it never returns an empty result
    that would read as "this box has no logs".
  - `ados diag storage` reports the store as disabled rather than as a store
    that exists and holds zero bytes — a different and much more alarming claim.
    The verdict no longer collapses to `unknown`, because the write rate is
    measured directly now.
  - A healthy verdict no longer says "no throttle events recorded" when the
    store is off. The sticky power and thermal bits are recorded nowhere else,
    so with it off nobody looked, and claiming a clean history for something
    unread is the fabrication this surface exists to refuse. It now says the
    history was not checked and why.
  - The resource routes' fallback to a direct host read is now covered by a test
    that asserts against the real host rather than a fixture. That path used to
    run for a few seconds at boot; it is now the only path on most nodes,
    forever, and a gap in it would blank every CPU, memory and disk reading on a
    normally-configured box.

||||||| 6823f261
## [0.99.342] - 2026-08-03

### Fixed

- **The storage diagnostic depended on the logging store, which is about to be
  off by default.** It read the write counter out of the store's hardware
  snapshots, so the one tool that made the dying-card diagnosis possible would
  have gone blind at exactly the moment it was needed most — on a node whose
  store was stopped, torn, or turned off.

  It now takes the reading itself: two `/proc/diskstats` samples five seconds
  apart, differenced. That needs nothing but the kernel. The store is still
  read, because its retained window is hours long where this one is seconds and
  because the sticky throttle bits exist nowhere else, but it is no longer the
  only source and no longer a prerequisite.

  The direct reading wins when both exist, and the output says which one is on
  screen. Five seconds of counter and hours of average answer different
  questions — the first shows whether a change made a minute ago worked, the
  second whether the box is like that all the time — and an operator acting on
  one while reading the other draws the wrong conclusion.

  The honesty rules are unchanged and now cover a third case: a counter that
  went backwards between samples (a device removed and re-added) is skipped
  rather than becoming an enormous rate, a `/proc/diskstats` line too short to
  carry the counter is skipped rather than read as zero writes, and a box with
  neither source available reports an absent rate naming both failures. A
  genuinely idle card still reads as zero, which is a measurement and a
  different claim from "could not measure".

## [0.99.341] - 2026-08-03

### Changed

- **Named the plugin-log size cap where the log is written, not only where it is
  enforced.** systemd has no directive that bounds an `append:` destination, so
  the per-plugin logs are capped from outside by the supervisor's janitor. That
  left the two halves of one rule in two crates with nothing connecting them: a
  reader of the unit generator would see an append path with no limit and
  reasonably conclude there was none.

  The unit generator now declares the cap and the log suffix as named constants
  beside the path it writes, explains that a trim has to rewrite the file in
  place because systemd holds that descriptor open for the life of the plugin,
  and carries a test asserting the log still lands in the directory and under
  the suffix the janitor selects on. Moving either without moving the janitor
  now fails a test instead of quietly unbounding the logs again.

## [0.99.340] - 2026-08-03

### Added

- **`ados diag storage` now says what is sitting on the card, not only how fast
  it is being written.** The wear figures answer one half of why a card fills;
  the ground station whose card filled was not writing quickly at all, it was
  holding 349 MB of downloaded packages nothing ever removed. That half was
  invisible.

  A new section reports the janitor's last pass — which rung it ran at, how long
  ago, and the bytes it freed broken down by category — followed by what a full
  pass would still be able to free, again per category. The second figure is
  what a reclaim would actually take, not the raw size of each directory: most
  of a log is not reclaimable because its tail is kept, and most recordings are
  not reclaimable because the newest survive at any age, so reporting footprints
  would promise space that does not exist.

  A box whose janitor has not run reports that, rather than a column of zeroes.
  "There is nothing left to reclaim" and "nobody has looked" are different
  answers and only one of them means the card is fine. The pass age travels with
  the figures so hour-old numbers are visibly hour-old.

## [0.99.339] - 2026-08-03

### Added

- **An hourly disk janitor, because five separate things on this box grew with
  nothing anywhere reclaiming them.** The installer fix returns the space apt
  borrowed once; this is the half that runs forever. It lives inside the
  supervisor alongside the seven reconcilers already there rather than as a new
  service, since a new service is one more thing that can fail to start.

  Three rungs, chosen from free space where `/var` lives. Routine, every pass,
  takes back what is unambiguously waste: the apt archive cache, the part of any
  plugin log or of the audit trail past its size cap, and recordings past their
  retention. Under pressure (below 20% free) it also gives up the apt package
  index, vacuums journal history, tightens recording retention, and prunes
  quarantined copies of a store that tore. Below 10% free it does all of that
  and says so loudly, because at that point the box is close to the state that
  ends in a card which will not boot.

  Two rules hold at every rung, because a janitor that quietly deletes evidence
  is worse than the full disk it was fitted to prevent. Nothing is reclaimed
  without being recorded: each pass emits one event carrying the bytes freed per
  category, so "the janitor ran and found nothing" can be told apart from "the
  janitor did not run". And every category has a floor: the newest quarantined
  store survives even the most aggressive pass, since it is the evidence of the
  most recent corruption; each log keeps its tail; the newest recordings survive
  however old they are; the journal is never vacuumed below a minimum. The
  config, the radio keys and the installed runtime are refused outright at the
  single removal helper, so no category can reach them by mistake.

  A free ratio the box could not measure resolves to Routine, never higher.
  Being unable to read the filesystem is not evidence that space is short, and
  the escalated rungs give up things that have value.

  Tunable under `storage.janitor`; on by default, and an unreadable config
  leaves it on rather than silently disabling a safety net.

### Fixed

- **The retention policy for append-only files could never have worked.** The
  drop-in that ages out plugin logs, the audit trail and recordings works on
  file age, and a file being continuously appended to is never old — its
  timestamps are refreshed by every write. So it correctly aged out recordings,
  which are closed when the capture ends, and could never age out either of the
  two files it was mostly written for. The audit trail additionally lives beside
  the agent's other persistent data rather than under `/var/log`, which is the
  directory the drop-in names. Both are now bounded by size, which is a property
  an open file actually has.

  The trim rewrites the file in place rather than renaming it, because systemd
  opens a plugin log once when the unit starts and holds that descriptor. A
  rename would leave every later write going to the renamed file, so the
  rotated log would keep growing under its new name while the new one stayed
  empty forever.

## [0.99.338] - 2026-08-03

### Fixed

- **The largest thing on a freshly flashed ground station was downloaded
  package files nobody would ever open again.** Measured on a rig two days
  after a flash: 349 MB across the apt archive cache and the package index,
  ahead of the logging store, the journal and everything else on the card.
  `apt-get install` copies every `.deb` it fetches into
  `/var/cache/apt/archives` and leaves it there permanently; nothing in the
  installer, the agent, or the base image had ever run `apt-get clean`.

  The installer now reclaims both, immediately after the packages are
  installed rather than at the end, so the space is back before the virtual
  environment, the fetched binaries and the driver build ask for it. The
  archive cache is pure waste and goes unconditionally. The package index is
  not waste, so its removal is a deliberate trade: the next apt invocation has
  to run `apt-get update` first, which every apt path the agent owns already
  does, and which apt itself tells a human to do. One command against a third
  of a gigabyte on a card that had been filling until it corrupted.

  The step cannot fail an install. A reclaim that does not work is a reason to
  try harder later; a full disk is the condition it exists to relieve, so
  aborting on it would be backwards.

## [0.99.337] - 2026-08-03

### Fixed

- **A drone transmitting into a void reported "all hops flowing".** The video
  diagnostic attached a fixed sentence — "RF reception is confirmed on the
  receiver's link_diag" — to the radio-injection hop whenever any byte was being
  injected. It never asked the receiver anything. It also counted that hop as
  flowing, so the summary line agreed.

  Found on hardware: a drone injecting 296 KB/s at a ground station that had been
  reflashed, held no radio key at all, and was decoding nothing. The tool said
  the link was healthy. The same agent's link diagnostic, on the same box in the
  same session, correctly said the link was unverified — so the tool disagreed
  with itself, and the optimistic half is the one printed first.

  The hop now reads the reception verdict the radio already computes and reports
  what it found: reception confirmed, reception UNCONFIRMED, or no verdict
  available. Injecting without confirmed reception is not a flowing hop — it
  resolves as unknown, which names that hop as where video dies. The byte count
  is still shown, because the transmitter genuinely is working; what changed is
  the claim about the far end. An advancing transmit counter proves a
  transmitter, never a receiver.

- **Auto-pair could stop trying and never resume.** The loop exited permanently
  in two places: after a successful pair, and after the attempt cap flipped a rig
  to the cloud relay. Both meant the recovery path could only ever run once, at
  boot.

  The second one is the more damaging: a ground station that gave up after the
  cap was no longer in a bind window when its drone returned, so the two could
  not meet again without a restart. The loop now stays alive in both cases — a
  paired rig answers "no" cheaply on each tick, and a cloud-parked rig stops
  spending local attempts without ending the loop.

### Fixed

- **The I2C status OLED crash-looped on every ground station, always.** The
  service panicked inside its own logging setup — "there is no reactor running"
  — five times in a row and then gave up, so the display never painted anything.
  Found by looking at what a rig actually reported after an upgrade rather than
  by anyone using the OLED.

  The logging layer spawned its background shipper with `tokio::spawn`, which
  requires an ambient async runtime, and documented that as a caller
  requirement. Every other binary happens to have an async `main`; this one is
  synchronous, because driving an I2C panel does not need a runtime. So the
  panic landed in the one binary where the requirement was not met, and it
  landed during tracing init — before any log line could say so.

  A logging layer cannot reasonably impose that contract: logging is set up
  first, before the process has decided what shape it is. The layer now runs its
  shipper on the ambient runtime when there is one and on its own thread when
  there is not, so it works from any binary. The regression test is deliberately
  not an async test, because the whole point is the absence of a runtime.

### Fixed

- **Per-core CPU frequency and utilization were the largest thing on the card.**
  With the storage diagnostic finally able to answer the question, a live node
  writing 1 034 KB/s showed where it was going: of 119 stored rows a second, 108
  were metrics, and 62 of those were sixteen keys — frequency and utilization for
  each of eight cores — sampled about four times a second and stored every time.

  Nothing reads that series at that resolution. The headline
  `cpu.utilization_pct` is a separate 1 Hz aggregate, and the per-core detail
  exists to show load imbalance and pinned cores, both of which are tens of
  points wide. What was actually being recorded, forever, was the sampler's own
  jitter.

  These now go through the same change gate the thermal series already uses: a
  row when the value moves (1 MHz for a clock, which steps between discrete
  operating points; 5 percentage points for utilization, which wanders several
  points doing nothing), or when the signal has been quiet for 30 seconds so a
  flat reading is never mistaken for a dead producer. The live snapshot keeps
  carrying the current value on every tick — it is one row for the whole box, so
  freshness there is free — and only the stored series is gated.

  The write rate on hardware after this change is measured, not predicted, in
  the notes for the following release.

## [0.99.334] - 2026-08-03

### Added

- **`ados diag storage` — read back the wear the box was already recording.**
  Four SD cards were replaced in eight days without anyone being able to say how
  much the node was writing. The measurement existed the whole time: the hardware
  collector puts the disk write counter and the throttle bitfield into the
  durable store on every tick, and that store survives the reboot that destroys
  everything in RAM. Nothing read it back.

  A new `GET /api/diag/storage` does, and the CLI renders it. Three things it is
  careful about:

  - The write counter is reported as a **delta across the retained window**, not
    as a reading. A cumulative counter's instantaneous value says nothing about
    rate. If the counter went backwards the node rebooted mid-window, and the
    answer is "not computable across a restart" rather than a negative number.
  - The throttle bitfield is reported by its **sticky "has occurred" bits**.
    Undervoltage is transient, so a single poll almost always misses it — which
    is why the one reading ever taken proved nothing.
  - Every field can come back absent **with a stated reason**, and a store that
    did not answer reads `unknown`, never `ok`. A fabricated zero here would look
    like a clean bill of health on exactly the card that is dying.

  It also totals the store's own footprint, including quarantined copies of a
  torn store — those are renamed aside rather than deleted, so a node that has
  corrupted twice carries three, which is the mechanism that filled the card.

### Fixed

- **A subprocess that failed immediately took its error with it.** The stderr
  drain summarises rate-limited output only when the *next* window opens, so a
  child that died inside its first window logged its banner and nothing else —
  everything past the limit was silently lost. That is the worst case to lose:
  a subprocess failing immediately is failing at startup, which is exactly when
  its output matters. An encoder fault that had to be reproduced by hand had
  logged nothing but a banner for this reason.

  The summary is now also flushed when the stream closes. `drain_plain` returns
  what it did (`logged` / `suppressed`), so the behaviour is assertable without
  standing up a tracing subscriber — the test drives a child that outruns the
  rate limit and dies in the same window, and it fails if the flush is removed.

## [0.99.332] - 2026-08-03

### Fixed

- **`ground_station.display.type` did nothing.** The field was documented in
  three places as selecting which surface owns the panel, and was read by
  nothing at all — setting it to `none` still started the HDMI kiosk. It is now
  honoured: `lcd` and `none` both mean "this service does not own the screen",
  and the kiosk stands down cleanly before it touches the display, saying which
  selection stood it down. Read defensively, because this service has to start
  against a config written by an older or newer agent: a missing or unusable
  block means "no opinion", never a crash.

### Removed

- **`ground_station.display.detected_type`**, which was always `null`. It was
  documented as "populated by the heartbeat enrichment helper after probing what
  the OS actually exposes" — but no such helper was ever written, nothing ever
  assigned it, and the heartbeat field it was meant to feed is hardcoded `None`
  in two places. A read-only field that is permanently null while three
  docstrings describe it as live is a lying surface. What the agent actually
  detected belongs in runtime status, not in the config file an operator edits.

## [0.99.331] - 2026-08-03

### Fixed

- **A single parameter write could put an aircraft permanently beyond the
  agent's reach.** The param and command paths both address vehicle system id
  `1`, so writing `SYSID_THISMAV` made the flight controller stop answering
  every subsequent `PARAM_SET` from this agent — **including the write that
  would undo it** — and stopped `arm`, `disarm`, `mode` and `rtl` reaching it
  too. There is no reboot route on this surface either, so an operator could not
  cycle out of it. One request, irreversible, and nothing about it looks unusual
  at the time.

  Writes to `SYSID_THISMAV` / `MAV_SYS_ID` are now refused with the reason
  named, pointing at a direct USB parameter tool for the case where a
  non-default system id is genuinely wanted. The guard is deliberately narrow —
  `SYSID_MYGCS` (which GCS may command us) and `SYSID_ENFORCE` stay writable,
  and there is a test for that, because refusing a benign parameter would be its
  own bug.

  Lifting the limit properly requires
  discovering the vehicle's system id from its `HEARTBEAT` and carrying it
  through both paths with a per-vehicle record. Merely widening the constant
  would replace an obvious failure with a subtle one, where a command silently
  reaches the wrong airframe.

## [0.99.330] - 2026-08-03

### Fixed

Two faults that let the black panel in `0.99.329` look healthy for as long as it
did. Neither caused it; both hid it.

- **The child's stderr was only read when the child exited.** A compositor that
  stays up with a broken browser inside it never exits, so the error was never
  read and never logged — the journal's last line was `kiosk_child_running` and
  the GPU-initialization failure was only visible by running the argv by hand.
  stderr is now streamed to the journal line by line *while* the child runs,
  bounded at 40 lines so a chatty browser cannot flood a flash-backed journal,
  and the rolling tail the GPU-downgrade heuristic reads is kept live rather
  than assembled at death.

- **The supervisor watched the compositor, not the browser.** On the appliance
  path the supervisor's child is `cage` and the browser is its grandchild, so a
  dead browser under a live compositor was invisible to `proc.wait()`: unit
  `active`, zero restarts, child alive, nothing on screen. The browser is now
  watched directly, and its disappearance is treated as a crash so the existing
  backoff and GPU-downgrade machinery handles it instead of a black screen
  reading green.

  The probe errs deliberately toward "still running" on any uncertainty (no
  `pgrep`, a permission error, an unexpected exit status): a false negative
  restarts a *working* kiosk, which is worse than missing one failure. It also
  waits out a start grace period, because the browser is spawned by the
  compositor and is legitimately absent for a moment after launch, and it is
  inert on the windowed path where the child already *is* the browser.

## [0.99.329] - 2026-08-03

### Fixed

- **The HDMI panel was black on a working ground station, because of a Chromium
  flag that was renamed upstream.** The kiosk passed `--use-gl=egl` on the GPU
  path. That spelling has since become `--gl=`, and the old one does not fail
  loudly — it resolves to "no implementation", so Chromium's GPU process exits
  during initialization, again and again, while `cage` stays up holding the
  display. Every health signal read fine: the unit was `active`, zero restarts,
  the supervisor's child was alive, the journal's last line was
  `kiosk_child_running`. The screen was simply black.

  Audited on the affected board (Chromium 150), all under `cage`::

      (no flag)               gpu_process_exits=0
      --gl=egl-angle          gpu_process_exits=0
      --use-angle=gl          gpu_process_exits=0
      --use-angle=gles        gpu_process_exits=0
      --use-gl=egl  (ours)    gpu_process_exits=4   <- the only failing option

  The GPU path now names **no** GL implementation at all. Several spellings
  work, but only naming none of them cannot go stale the same way, and
  Chromium's Linux default is already the ANGLE/EGL path the flag was trying to
  request. `--enable-gpu-rasterization` is dropped for the same reason: it has
  been the default for years, and carrying flags whose behaviour is now the
  default is precisely how the broken one survived long enough to matter.

  The software path is unchanged (`--disable-gpu`), so a board that genuinely
  cannot drive a GPU is unaffected.

## [0.99.328] - 2026-08-03

### Security

- **A paired node served its live video to anyone on the LAN, with no
  credential.** The proxy exempted every path outside `/api/` from its
  credential check — a rule written for the static SPA, whose client-side routes
  genuinely are not enumerable — but `/whep` and `/hls` are a live data plane
  that happens to sit outside `/api/` by URL shape. The effect was a **paired**
  node being *looser* than the same node while **unpaired**, where
  `auth::is_operator_ui` refuses `/whep` deliberately and says why.

  The media plane is now never exempt. The SPA's route space is untouched, so
  the operator UI still loads on any path — there is a test for each half,
  because narrowing this carelessly would take the whole UI down to fix a video
  leak.

  This is shipped **together with its client half**, in this order on purpose:
  all three video clients (cockpit WHEP, dashboard WHEP, dashboard HLS) now send
  the same two credentials `apiFetch` already sends. `hls.js` fetches the
  playlist and every segment itself, so it needed an `xhrSetup` hook rather than
  a header on one call — the absence of that hook is why the paths could not be
  gated before now.

### Fixed

- **Two layers disagreed on what a valid unpaired session is.** The unpaired
  edge validated with the empty-key issuer while the proxied path used the
  paired-key one, which returns `false` when unpaired. Harmless only by
  accident — an unpaired node's data plane was open anyway, so the stricter
  check was never reached. It is no longer open, so both now go through one
  predicate that dispatches on the actual pairing state.

## [0.99.327] - 2026-08-03

### Fixed

- **The hardware collector was storing one physical sensor twice, and storing
  every sample of a temperature that had not moved.** Measured in steady state
  on a real node: **165 metric rows/sec across 83 keys, and the top sixteen keys
  were all `thermal.*`** — a board with ~16 thermal zones sampled at 200 ms,
  landing on flash, for a store whose own rollups are minute-grained.

  Two causes, both fixed:

  - **Duplication.** A thermal zone backed by a hwmon device appears in *both*
    `/sys/class/thermal` and `/sys/class/hwmon`, and both were recorded —
    `thermal.skin_zone_c` and `thermal.hwmon.skin_zone_temp1_c` were the same
    sensor at the same cadence. A hwmon chip that does not correspond to a zone
    is still recorded; only the overlap is dropped.
  - **Storing stillness.** The fast cadence is deliberate — a thermal transient
    is the canary for a throttle — but *sampling* fast and *storing* every sample
    are different things, and only the second costs flash. A reading is now
    stored when it actually moved (0.5 °C, above the sensor's idle jitter) or
    when the signal has been quiet for 30 s. A transient still lands on the very
    sample that sees it, which is the property the cadence exists for, and the
    heartbeat keeps a flat signal from reading as a dead producer while
    guaranteeing every one-minute rollup bucket has a sample.

  The snapshot is untouched: every reading still goes into it unconditionally,
  because that is the live view and it costs one blob. Only the per-signal rows
  are gated.

## [0.99.326] - 2026-08-03

### Fixed

- **The periodic `VACUUM` was a scheduled opportunity to create an unrecoverable
  start.** A `VACUUM` runs as a single transaction, so `wal_autocheckpoint`
  cannot fire inside it and the entire rewrite lands in the WAL. On a ~1 GB
  store that produced a **955 MB WAL** on a real node, and the next start had to
  recover it — inside a `TimeoutStartSec` whose expiry would restart the daemon
  straight back into the same recovery. It came up with seconds to spare; a
  larger store would not have.

  Now that reclaim is incremental, the full `VACUUM` has almost no job left, so
  it runs only when warranted: a store that has not adopted incremental mode yet
  and needs the one-time conversion rewrite, or a file whose free list has grown
  past a quarter of it (incremental reclaim genuinely falling behind). A healthy
  incremental store never rewrites itself, which removes the last whole-file
  rewrite from the steady state — and with it the only thing that was producing
  gigabyte WALs.

## [0.99.325] - 2026-08-03

### Fixed

- **An upgrade could silently change what a box IS, and the documented
  mitigation did not exist.** `ados update` ran `install.sh --upgrade` with no
  profile at all, leaving the installer to resolve it. A gap in that resolution
  has re-profiled a live ground station to `drone` and left it in a reboot loop
  that needed a reflash to recover — and the standing advice, "always pass
  `--profile` on `ados update`", was unactionable: the command had no such
  option (`--check-only`, `-y/--yes`, `--json` only).

  `ados update` now reads `/etc/ados/profile.conf` — which both `install.sh` and
  the wizard already write — and passes it through explicitly, so an upgrade
  states what the box is rather than trusting a default to be right. It prints
  the pinned profile, and says so plainly when no marker exists instead of
  passing silently. A `--profile` option is added for the one legitimate case:
  deliberately converting a box from one role to another.

  Tested at the argv level, not just the decision: removing the pass-through
  fails the test. Asserting only on the computed value would have missed exactly
  the bug being fixed.

- **A failed install left the box with no installer.** The edge path deleted
  `/opt/ados/source` *before* cloning, so a clone that failed for any reason
  left no source tree — and that tree carries `scripts/install.sh`, so the node
  lost the ability to retry its own install. On a node with no internet that is
  unrecoverable. Observed on a rig: the clone failed and
  `bash /opt/ados/source/scripts/install.sh` was afterwards "No such file or
  directory".

  The clone now lands in a sibling staging directory and is promoted only once
  it succeeds, so a failure leaves the existing tree exactly as it was. The
  staging directory is deliberately a sibling, never a child — a child would be
  destroyed by the very removal that makes room for the promotion.

## [0.99.324] - 2026-08-02

### Fixed

- **`0.99.319` put a whole-file rewrite in front of daemon readiness, and it was
  minutes from crash-looping a real node.** Adopting incremental auto-vacuum on
  a store created before that mode existed needs one `VACUUM`, and that
  conversion was done inside `db::open`. On a node with a 950 MB store the unit
  — `Type=notify`, `TimeoutStartSec=5min`, `Restart=on-failure` — sat in
  `activating` for minutes accumulating a **955 MB WAL**, heading for a
  start-timeout kill mid-rewrite followed by a restart into the same rewrite.
  That is a crash loop that tears the store: precisely the failure the
  incremental work exists to prevent.

  The conversion now rides the **periodic** `VACUUM` instead — the one rewrite
  that was going to happen anyway — so it costs nothing extra and never blocks
  startup. Until a legacy store converts, `incremental_vacuum` is a no-op on it
  and retention honestly reports `reclaimed_pages: 0` rather than pretending.

  Caught by deploying `0.99.323` to a rig and watching it, not by a test. There
  is a test now, and it fails against the shipped-and-wrong version.

- **A *fresh* store was not getting incremental mode either.** SQLite only
  honours `auto_vacuum` while the database is still empty, and setting
  `journal_mode` is itself enough to establish the file header — so applying the
  pragmas in the written order left every new store in the default mode, making
  `incremental_vacuum` a permanent no-op on it. `auto_vacuum` is now set first.
  The ordering is load-bearing, not stylistic.

## [0.99.323] - 2026-08-02

### Fixed

- **A freshly installed node's dashboard was a dead end, and it blamed the
  hardware.** Visiting a new agent by IP showed *"Agent unreachable — check that
  the board is powered, on the network, and that `ados-supervisor` is running"*.
  Every clause of that was false: the agent answered in milliseconds with
  `403 This device is not paired yet. Set up access with the dashboard PIN, or
  pair it first.`

  The access gate only treated **401** as a challenge. That is the *paired*
  case — reached off-box without a credential. An unpaired node on the operator's
  own LAN answers **403**, so the gate fell through to "render the app anyway",
  every panel then failed, and the generic offline card claimed the board was
  unreachable. The screen that should have appeared — the branded PIN splash,
  which already knows how to offer *setting* a PIN on a node that has none —
  existed and worked, and was simply never reached.

  Both codes now count. Verified end-to-end against a real unpaired agent:
  `403` → challenge → `pin_set: false` → the splash opens in set-a-PIN mode.

- **The offline card no longer blames the board for a board that answered.**
  "Did not answer" and "answered, and said no" are different faults with
  different fixes, and reporting the second as the first sends the operator to
  check a power cable about a node that is running fine. When the failure was a
  refusal, the card now says so and quotes the agent's own words.

### Added

- Unit tests for the dashboard, which had no test framework at all. Mirrors the
  cockpit's vitest setup, and CI now runs them.

## [0.99.322] - 2026-08-02

### Fixed

- **Dead copies of a corrupted logging store were only reclaimed by the next
  corruption.** Pruning ran as part of quarantining, so a node that corrupted
  twice under a build predating the prune carried both copies — up to two
  gigabytes of unreadable file — until it corrupted a *third* time. That is the
  worst moment to be short of space, and being short of space is a fair way to
  cause it. A bench node was carrying 1.6 GB of exactly this beside a live
  935 MB store. Pruning now also runs on a healthy start.

- **Three on-disk outputs had no retention at all**: the per-plugin logs
  (systemd `StandardOutput=append:`, with no rotation configured anywhere), the
  audit trail, and operator flight recordings. None is a fast writer, so none
  was part of the write load — but the failure they lead to is a full root
  filesystem, which is how a recoverable problem becomes an unbootable card,
  and nothing else on the box reclaimed them. A `systemd-tmpfiles` drop-in now
  ages them out (14 days for plugin logs, 90 for the audit trail and
  recordings). `systemd-tmpfiles` rather than logrotate because it is already
  used for the plugin runtime directory and needs no extra package.

- **`ados uninstall` left the box a headless appliance.** Masking is a symlink
  to `/dev/null` in `/etc/systemd/system`; it is not a file under `/opt/ados`
  and deleting the agent's drop-ins never undid it. Removing the agent left
  `display-manager.service`, `lightdm.service` and the five sleep targets
  masked, with nothing on the machine left to explain why. Uninstall now
  unmasks them, and a test compares the list against the masking sites
  themselves so a future mask cannot silently become permanent.

### Measured, and deliberately not changed

- **journald is not a meaningful writer here.** It was a suspect, so it was
  measured rather than tuned on a hunch: 163 entries in a 60 s window on a live
  node, on the order of tens of megabytes a day against the ~144 GB/day the
  store was doing before `0.99.319`-`0.99.320`. `Storage=persistent` also earns
  its place — it is what keeps an oops trace across the reboot. Left alone, and
  recorded here so it is not "optimised" later on the same hunch.

## [0.99.321] - 2026-08-02

### Fixed

- **A blocked task rebooted the box, which is how a slow card becomes an
  unbootable one.** The installer set `kernel.hung_task_panic = 1` alongside
  `kernel.panic = 10`, on the reasoning that the kernel-default 120 s timeout
  meant a busy I/O path would never false-trip. That does not survive contact
  with an SBC writing to an SD card: 120 s in uninterruptible sleep is
  reachable whenever storage is saturated, and the reboot lands *during* a
  write. Slow storage then becomes a damaged filesystem, which is slower still,
  which trips the timeout again.

  `hung_task_panic` is now off, written as an explicit `0` so an upgrade
  actively reverts a node that already has it on rather than waiting for a
  reboot to fall back to the default. `panic_on_oops` and the hardware watchdog
  are unchanged — an oops means the kernel is already untrustworthy and
  rebooting is right. The 120 s timeout stays, because it is what puts the
  "blocked for more than 120 seconds" warning and its stack in the journal:
  the evidence the reboot used to destroy.

- **The kiosk browser wrote an unbounded cache to the card, continuously, for
  the life of the box.** Nothing pinned Chromium's storage, so under `cage` —
  where the service runs as root — its HTTP cache, code cache, shader cache,
  cookies, history and Local Storage went to `/root/.cache/chromium` and
  `/root/.config/chromium`. The page it shows is a live-updating SPA carrying a
  video stream, and a ground station shows it permanently.

  Profile and cache now go to a tmpfs directory with a 64 MiB cache cap, and
  die with the boot. None of it was worth persisting: the kiosk shows one page
  served from localhost, with no login and no session to carry across a reboot.
  The cap matters *because* the target is tmpfs — an unbounded cache there
  would trade SD wear for RAM exhaustion on a board sharing memory with the
  video pipeline.

  The windowed (in-desktop) path gets the same treatment under the session
  user's own runtime dir, which fixes a second thing: the kiosk no longer
  shares a profile directory with the operator's own browser, so launching it
  can no longer collide with a Chromium they already have open.

## [0.99.320] - 2026-08-02

### Fixed

- **The logging store recorded ~15 million telemetry rows a day that nothing
  reads.** The state tap lifted ~17 numeric fields out of every snapshot on a
  stream the state hub publishes at roughly 10 Hz, with no rate limit at all —
  while the raw-frame tap sitting beside it has been sampled at 1 Hz since it
  was written, for exactly the reason that applies here too. The store's own
  rollups are minute- and hour-grained, so one sample per second is already 60
  per bucket and the other nine tenths were landing on flash unread. Metrics are
  now sampled at 1 Hz (`state::DEFAULT_SAMPLE_HZ`, tunable per tap).

  Transitions are deliberately **not** sampled. An arm, a disarm or a mode
  change is a discrete fact that drives the flight-session bookkeeping; dropping
  one loses it for good, unlike a metric the next snapshot carries again.

- **The hardware collector wrote a snapshot row 10 times a second forever, even
  with nothing to report.** Every signal class is slower than the 100 ms base
  tick, so most ticks have nothing due, and `emit` has always had an
  empty-snapshot guard for exactly that. The guard never fired: `soc.compat` — a
  constant read once at construction — was inserted at the top of every tick,
  before any cadence check, which made every snapshot look like a reading.
  ~864 000 rows a day. The constant now folds in at the end of the tick and only
  when a class actually reported, so it still rides along on the snapshots that
  are emitted without manufacturing the ones that are not.

## [0.99.319] - 2026-08-02

### Fixed

- **The logging store rewrote its whole ~900 MB file every 45 to 100 minutes,
  forever.** `retention.rs` ran a full `VACUUM` after *every* size-cap eviction,
  not just on its documented weekly cadence. Once the store reaches its 1 GB cap
  — roughly 5 to 10 hours after install — eviction triggers on that cadence for
  the life of the box, so the rewrite did too.

  Measured on a ground-station-class board: **1 714 KB/s sustained writes to the
  card, about 144 GB a day**, on a node doing nothing but running. That is one to
  two orders of magnitude above what a 24/7 SBC should write, and it is the
  dominant write load on the whole system. A rewrite of that size is also a
  minutes-long window in which a power cut tears the store; two abandoned
  `logs.db.corrupt-*` files, 1.6 GB between them, were sitting next to a live
  935 MB store on a bench node when this was found.

  Reclaim on the eviction path is now incremental: the store opens in
  `auto_vacuum=INCREMENTAL`, evicted pages go on SQLite's free list where
  subsequent inserts reuse them, and a bounded `PRAGMA incremental_vacuum`
  (16 384 pages, ~64 MiB) returns the surplus. The full `VACUUM` stays, but only
  on its own long cadence. An existing store converts itself once at open, with
  a log line, since SQLite only honours the mode change across one rewrite.

  The size cap moves with it, and had to: it now arms on the **logical** used
  size plus the WAL rather than the raw file size. A store whose freed pages are
  being reused legitimately sits at its high-water file size, so keeping the old
  trigger while no longer shrinking the file would have re-fired eviction on
  every pass and drained the store to empty. A regression test drives three
  consecutive passes over a store parked at the cap and asserts only the first
  one evicts.

## [0.99.318] - 2026-08-01

### Fixed

- **`scripts/probe-display-planes.sh` could not actually run its own
  `--under-cage` path, and its verdict there was wrong.** Five defects, all found
  by running it on the ground station rather than by reading it:
  - **cage exited immediately.** wlroots refuses to create its socket without
    `XDG_RUNTIME_DIR`, and `sudo` strips it, so every under-cage arm died with
    `XDG_RUNTIME_DIR is not set` and reported "the client never displayed
    anything". The probe now supplies the same `/run/user/0` the `ados-kiosk`
    unit sets, creating it when absent.
  - **it could measure the wrong GPU.** The DRM node was the first readable
    `state` file, which on a Pi 4 can be the render-only node that exposes no
    planes at all. It now picks the node that actually has planes, and pins
    `WLR_DRM_DEVICES` to the matching card so cage drives the device being
    measured instead of autodetecting a different one.
  - **the under-cage verdict was a false pass waiting to happen.** With
    `--under-cage` the compositor starts *inside* the measurement window and
    binds its own plane, so every arm reads at least +1 — including a `wl_shm`
    client that a DRM backend physically cannot scan out. Comparing against the
    pre-client count would have printed "DOES promote" for a client holding CPU
    memory. The under-cage path now requires `--baseline <n>`, the `during`
    value from a `wl_shm` control run on the same box and renderer, and reports
    `INDETERMINATE` rather than guessing when it is missing.
  - **it orphaned a compositor.** Killing only the launcher PID left `cage`
    running and holding DRM master; the probe then blocked forever in `wait`,
    the kiosk could not restart, and the panel was left with a stray compositor.
    The client is now started with `setsid` and torn down as a process group,
    with a bounded TERM-then-KILL. The group id is taken from the child pid
    rather than read back with `ps`, because that read races the `setsid` and
    can return the probe's own group — killing the probe, its shell and the
    operator's session.
  - **the `glsrc` arm never ran.** `glimagesink` has no `fullscreen` property
    (that is `waylandsink`), so gst-launch rejected the pipeline and the arm
    reported a client failure instead of a measurement.

## [0.99.317] - 2026-08-01

### Added

- **The display-plane probe can now ask the question it was written for.**
  `scripts/probe-display-planes.sh` only ever ran a `videotestsrc ! waylandsink`
  client, which hands the compositor `wl_shm` CPU memory. A DRM backend can never
  scan that out, so its "no plane" answer was a property of the client, not of the
  compositor — the underlay was never actually tested. The probe gains
  `--client <testsrc|glsrc|rtsp>` (`testsrc` unchanged, so the first measurement
  stays reproducible byte for byte; `glsrc` renders through Mesa EGL for real
  dmabufs with no decoder; `rtsp` hardware-decodes the live H.264 stream through
  `v4l2h264dec capture-io-mode=dmabuf`), `--under-cage` to run the client in its
  own `cage` with DRM master, `--renderer <pixman|gles2>`, and `--url`. Two new
  guards keep a non-answer from reading as a measurement: the `rtsp` client must
  be seen pulling the stream (mediamtx `bytesSent` advancing with a reader
  attached, polled up to 15 s) before any plane count is believed, and the
  effective `WLR_RENDERER` is reported alongside the compositor because a
  software wlroots renderer is itself a reason promotion could not happen. There
  is deliberately no software-decode fallback: `videoconvert` emits system memory,
  a DMABuf caps filter after it cannot negotiate, and the broken pipeline would
  read as a measured "no" when nothing was measured at all.

- **Ground-station appliances stand the login manager down.** The kiosk needs
  `cage` to hold the DRM master to scan out, and a running desktop compositor
  holds it instead — so on a desktop-imaged ground station the kiosk silently
  resolved onto the windowed in-desktop branch. The installer's `appliance` step
  now masks `display-manager.service` and `lightdm.service` on the
  **ground-station profile only**. A drone has no desktop; a workstation or
  compute node stays a login box. Fail-soft: a mask problem degrades, never aborts
  the install.

  **Rollback — how to get the desktop back on a ground station:**

  ```
  sudo touch /etc/ados/keep-desktop
  sudo systemctl unmask lightdm display-manager.service
  sudo systemctl enable --now lightdm
  ```

  The `/etc/ados/keep-desktop` marker is what makes that rollback durable. This
  step has no checkpoint and therefore re-runs on every upgrade, so a bare
  `systemctl unmask` would be silently re-applied by the next `ados update`; with
  the marker present the installer never masks and actively unmasks anything it
  masked before. The marker is operator-created only — no config schema change.
  The in-place `systemctl stop` is skipped unless the installer's own logind
  session is known and non-graphical (the SSH/TTY case), because stopping the
  login manager from inside a graphical session would tear down the session the
  installer is running in and leave a half-configured box; when it is skipped the
  mask still takes effect from the next boot, and the installer says so.

### Fixed

- **The G3 Betaflight gate could not fail.** Its module contract says it requires
  "a matching body-rate echo", but the code accepted **any inbound byte** as the
  acknowledgement. Every flight controller streams telemetry continuously, so the
  gate passed regardless of whether the attitude command was accepted — a
  guaranteed false pass on a safety gate. It now accumulates inbound bytes,
  frames them, and requires an `ATTITUDE_TARGET` (id 83) carrying back the
  commanded `0.5 / -0.2 / 0.1` rad/s within `1e-2` before the 2 s window elapses.
  The FC tty is put into raw binary mode first, because a CDC-ACM port comes up in
  canonical mode where `read()` blocks for a newline and `ICRNL` rewrites `0x0D`
  to `0x0A` inside a binary MAVLink frame. On timeout the assertion reports the
  inbound byte count, the framed and decoded frame counts and the set of message
  ids seen, so "the FC is silent", "the port is not MAVLink", and "the FC speaks
  MAVLink but never echoes the attitude target" are distinguishable rather than
  one undifferentiated failure. The gate keeps its `#[ignore]` and its
  fail-closed posture with no `ADOS_G3_FC_PORT`.

## [0.99.258] - 2026-07-29

### Fixed

- **`mediamtx` UDP RTSP transport collided with `wfb_tx`'s control-port range.**
  mediamtx's default `rtsp: true` also opens UDP RTP/RTCP listeners
  (`rtpAddress`/`rtcpAddress`, mediamtx's own defaults `:8000`/`:8001`) for the
  `udp` RTSP transport, which nothing on this box uses (the encoder publishes
  over the TCP RTSP listener; WebRTC is the client-facing path). Those two UDP
  ports fall inside `ados-radio`'s `wfb_tx` control-port range
  (`TX_CMD_PORT_BASE=8000..8003`), so whichever service won the race at boot
  claimed the port and the loser crash-looped (`wfb_data_tx_exited_respawning`
  every ~3s when `mediamtx` won). `ados-video`'s generated `mediamtx.yml` now
  sets `rtspTransports: [tcp]`, so mediamtx never opens the UDP pair and the
  collision cannot occur regardless of start order.

## [0.99.255] - 2026-07-28

Fleet release: one ground station, one RTL8812EU per node, up to 24 drones on one
20 MHz channel. Single-drone behaviour is unchanged when the fleet has one member.

### Added

- **Fleet addressing.** `video.wfb.fleet_id` (u16, default 1) and
  `video.wfb.fleet_slot` (0 = ground station, 1..=24 = drones) compose into the
  wfb-ng `link_id` as `fleet_id << 8 | slot`, filling the 24-bit space exactly, so
  two fleets can share a channel with no `channel_id` collision. Every one of the
  eleven `wfb_tx`/`wfb_rx` argv builders now passes `-i <link_id>`, and every
  spawn and respawn path retains the ids it was keyed with so a channel hop or an
  FEC/MCS change re-keys identically. A wrong or missing slot fails the config
  load loudly and the node does not radiate, because a duplicate `channel_id`
  makes two transmitters re-init each other's FEC decoder about once a second,
  which presents as unexplained link loss rather than as a config error.
- **N receivers on one ground radio.** `wfb_rx` compiles a per-instance kernel BPF
  on `channel_id` onto a promiscuous, non-exclusive pcap handle, so one adapter
  carries every drone: the ground station now reconciles video (p0), aux (p2) and
  control (p1) receivers against the fleet registry, one set per registered slot,
  all bound to the same interface. Per-slot loopback egress at
  `VIDEO_RX_PORT_BASE + slot` / `AUX_RX_PORT_BASE + slot` /
  `CONTROL_RX_PORT_BASE + slot`, with the whole span guarded against an operator
  aux port. The uplink stays ONE transmitter on the ground slot that every drone
  receives, so a fleet-wide command is one transmission, not N.
- **Fleet registry** (`/var/lib/ados/fleet.json`): lowest-free-slot allocation,
  idempotent by device id so re-pairing never renumbers a flying drone, released
  slots reused, atomic temp-file-plus-rename persist.
- **One fleet key.** Pairing a second drone with a byte-identical key blob is
  accepted and issued the next free slot instead of being refused; the pair status
  returns the whole slot table. A differing blob is `E_FLEET_KEY_MISMATCH`, a full
  fleet is `E_FLEET_FULL`, and `E_ALREADY_PAIRED` is gone. A fleet is one trust
  domain by design — the swarm bus requires every drone to decrypt every other
  drone's beacon.
- **`ados-swarmbus`**: a decentralized state bus. Every drone broadcasts a
  20-byte beacon at 2 Hz with 0-100 ms of jitter, and every node — drones and the
  ground station — hears every other node with no ground station in the path. Own
  magic (`0xAD03`) in the position wfb-ng's BPF reads, one pcap handle and
  userspace demultiplexing rather than N receivers per aircraft,
  ChaCha20-Poly1305 under a fleet-shared key. 87 bytes on air, 0.84% of one
  channel at N=24. Neighbour table with staleness pruning, constant-velocity dead
  reckoning and k-nearest queries; `GET /api/swarm/neighbors` on both profiles.
- **`ados-swarm-control`**: onboard swarm autonomy. Separation, flocking
  (Olfati-Saber alpha-lattice), named formations and CBBA task allocation,
  arbitrated by a fixed precedence ladder -- hard separation > operator direct >
  formation > flocking > hold -- into `SET_POSITION_TARGET_GLOBAL_INT` setpoints
  at 10 Hz against dead-reckoned neighbour positions. Every control law is a pure
  function over a local NED frame, so the whole layer is testable with no radio,
  no socket and no flight controller. Adds no radio traffic of its own.
- `ados-mavlink-router` runs the loop (`connection::swarm_setpoint`): it reads the
  neighbour table off `/run/ados/swarm.sock`, interpolates the 2 Hz feed up to the
  10 Hz control rate, and commands the FC through the existing `send_msg` path
  only while the FC itself reports GUIDED. The active precedence level and the
  emergency condition are republished as the `swarm_precedence` /
  `swarm_emergency` snapshot keys, which `ados-swarmbus` folds into the outgoing
  beacon -- so `GET /api/swarm/neighbors` reports which layer is ACTUALLY flying
  each aircraft rather than which one it was commanded into.
- The five built-in formations (`line`, `column`, `wedge`, `grid`, `circle`) are
  generated from the slot set a drone can hear, so any fleet size from 1 to 24 is
  valid and a released slot leaves no hole in the shape.
- Four flight-gate scenarios run as ordinary tests and as
  `cargo run -p ados-swarm-control --example swarm_scenarios`: collision course,
  8-drone flocking to a 500 m target, wedge station keeping, and swarm-bus loss.
  Measured: minimum separation 4.14 m against the 4 m floor, 8/8 arrivals within
  4.5 m of target, 0.001 m steady-state station error, and a silent drone dropped
  after 2.5 s with zero setpoints emitted thereafter. This is the control-law
  gate, NOT the SITL gate -- software-in-the-loop with a real autopilot per
  aircraft is still required before flight.
- **Attention-based video.** A `thumbnail` encoder profile (320x180, 1 fps,
  50 kbps) beside the existing `hero` defaults (1280x720, 30 fps, 4000 kbps), both
  on the existing p0 pipeline and the existing `wfb_tx` — no new radio port, no
  second encoder, no new ground-side receiver. `POST /api/video/profile` on a
  drone; `POST /api/v1/ground-station/fleet/hero` promotes one drone and demotes
  every other registered slot concurrently, retrying each failure once and
  reporting 207 with per-slot outcomes rather than blocking the new hero's
  promotion. A channel-sharing node boots to `thumbnail` so a fleet powering up
  together never has 24 transmitters at 48% airtime each; a one-slot fleet is
  auto-promoted, preserving today's single-drone behaviour.
- **Live MCS and FEC control.** A `wfb_tx_cmd` client (`ados-radio::tx_cmd`)
  replaces the kill-and-respawn path, removing the 1-2 s video blackout per tier
  change. Bandwidth, GI, STBC and LDPC are passed through unchanged on every
  `set_radio` call, so a rate change can never silently retune the channel width.
  The respawn path stays as the fallback when the control socket is unreachable.
- **MCS now tracks measured SNR** instead of sitting statically at 1 (13.0 Mbps,
  the second-lowest rung) against a measured 35 dB at bench. Five rungs, 10 dB of
  margin held on each, down on two bad samples and up only after thirty good ones
  — losing range is instant and dangerous, gaining it is not urgent. Capped at
  MCS 3 by default because OpenIPC's production table tops out at MCS 2 in the
  field; anything higher is bench-only until the radio is characterised.
- The adaptive ladder now applies its tier bitrate to the encoder. It previously
  retuned FEC only, so a degrading link emitted MORE on-air bytes (rescue tier is
  3x FEC) on an already-strained channel. Ladder and profile compose through one
  applier as `min(profile, ceiling)`, so a hero on a bad link is clamped and a
  50 kbps thumbnail is never raised to the rescue tier.

### Changed

- **The FC parameter map is off the 10 Hz state publish.** `build_extras` inserted
  the entire parameter cache into every 100 ms snapshot with no delta and no size
  cap, which is why `/api/telemetry` measured ~25 KB and its relayed delivery sat
  at 85%. Replaced by a `param_generation` counter: 20 615 B -> 1 114 B with 700
  cached parameters, an 18.5x reduction, and the extras size no longer grows with
  the parameter count at all. `/api/params` and every other parameter reader now
  read the atomically-persisted `/var/lib/ados/params.json` instead.
- **RPC responses carry their sender and are RaptorQ-coded.** A broadcast reply
  previously completed a pending call on the first fragment matching a 32-bit id
  regardless of who sent it, so with N drones answering, two drones' fragments
  spliced into one buffer. Fragments now carry the sender device id and a
  mismatched sender is dropped and counted. Reassembly takes any k of k+4
  symbols instead of demanding every index: at the measured 0.7% per-fragment
  loss a 30 KB body goes from ~17% failure to under 0.001%, for 13% more bytes on
  air. `MAX_PENDING_CALLS` 256 -> 1024 and `MAX_PEERS` 8 -> 64 for a 24-drone
  fleet poll.
- Telemetry stream requests no longer duplicate. `tick_streams` sent both
  `SET_MESSAGE_INTERVAL` and the legacy `REQUEST_DATA_STREAM` every refresh on the
  assumption that a firmware honours one or the other; measured ingest of
  66.5 frames/s against 29 Hz asked for shows ArduPilot honouring both. The legacy
  loop is now behind `mavlink.legacy_stream_request`, default false.
- `swarm.lora` and its `LoraConfig` are deleted. No driver, no consumer, and LoRa
  is absent from the agent entirely, so the new swarm UI would have surfaced a
  field nothing reads. `swarm.flock.*`, `swarm.separation.*`, `swarm.tasks.*` and
  `swarm.mode` take its place, and `swarm.default_formation` is now a closed enum
  over the five built-ins instead of free text.

### Fixed

- The slot-indexed deconfliction climb measured its offset from each drone's OWN
  altitude, which does not deconflict: two vehicles at different heights can be
  commanded to the same altitude, and a pair held on a collision course ratchets
  ~40 m upward over 90 s, one step per re-engagement. Exactly one of a pair now
  climbs -- the lower slot, to a fixed offset above the HOLDER's altitude -- so the
  target is stationary across re-engagements and the ordering depends on slot
  alone. A vehicle already below its offender holds rather than climbing through it.
- The ground-station Atlas relay defaulted its listen port to the retired fixed
  aux egress 5603 while the aux lane now decodes per slot, which is the silent
  failure mode the port pin existed to prevent: frames arrive, decode fine, and
  land on a port nobody is bound to. The default now derives from the first drone
  slot's aux egress and the two are pinned together again by test.
- The parameter cache wrote on a flat 2 s debounce, which would have made the
  parameter-write acknowledgement a coin flip once `/api/params` began reading the
  file. A change arriving outside a `PARAM_REQUEST_LIST` sweep is now written
  immediately; the debounce still holds during a sweep, which is where the write
  volume is.

### Notes

- **Committed fleet size is 8 at MCS 1 and 24 only with the adaptive ladder
  holding MCS 3.** The airtime arithmetic does not close otherwise: one hero at
  48% plus 23 control-only drones is 103% of one channel at MCS 1. If the radio
  sweep cannot hold MCS 3 at real range, the honest size is 8 per channel.
- **The MCS rung table is arithmetic from standard 802.11 required-SNR figures,
  not a measurement of this driver.** The bench sweep procedure is in the WFB
  video pipeline runbook and must run before the ladder is trusted above MCS 3.
- Only MCS is varied at runtime. Channel width stays pinned at 20 MHz (the
  vendored RTL8812EU has no narrowband symbol compiled in, and 40 MHz has open
  upstream defects on this chipset) and TX power stays static (the existing
  ramp-until-accepted plus readback is evidence the driver honours only a couple
  of levels).
- The separation layer is enforced as a braking-aware closing-rate constraint
  applied last, not as an additive force. The plan's repulsive potential peaks at
  0.19 m/s as a neighbour reaches the 4 m floor, so a summed potential field is
  outvoted two orders of magnitude by any goal-seeking term and cannot hold that
  floor; "applied last and overrides" is implemented as an override.
- Flocking cohesion acts only on neighbours beyond the separation radius. An
  ungated cohesion gain against an independent repulsion gain puts the lattice
  equilibrium at 1.65 m, well inside the safety floor; gating restores the
  equilibrium to the separation radius with both of the plan's gains unchanged.
- `swarm.*` gains are integer percentages (`cohesion = 40` is 0.40) because the
  GCS config primitives have no float field. The conversion happens once, at the
  crate boundary.
- No leader election is built. The operator screen is the single authority by
  construction; electing a leader over a lossy broadcast invites a split brain
  that still hears peers but has lost the operator, for no benefit.

### Removed

- `E_ALREADY_PAIRED` on the ground-station pair route, along with its branches:
  refusing a second drone was the behaviour a fleet exists to lift.
- `ParamCache::get_all()` and `VehicleState.params`. The former's only production
  caller was the removed extras insert; the latter was a second in-memory
  700-entry map that nothing serialized and nothing read, costing one clone and
  one allocation per `PARAM_VALUE` frame for no reader.
- The fixed `DATA_RX_PORT` / `ATLAS_RX_PORT` / `RX_CONTROL_PORT` receive
  constants, replaced by the per-slot port helpers. Every consumer moved with
  them.

## [0.99.21] - 2026-06-29

### Added

- `POST /api/plugins/parse_from_url`: download an allowlisted `.adosplug`
  URL and return its manifest summary without installing -- the URL companion
  to the multipart `/parse`, so the GCS install dialog can review permissions
  before consent for an operator-supplied URL (the browser cannot fetch an
  arbitrary URL itself). Same allowlist + size cap + signature check as
  install-from-url; nothing is written to the plugin store.

## [0.99.20] - 2026-06-29

### Added

- The plugin host validates a config write against the plugin's declared
  parameter schema (`gcs.contributes.parameters[key].schema`, read from the
  installed manifest) before persisting it -- JSON Schema Draft-07 via the
  jsonschema crate, the agent half of the shared validator, so the
  agent never trusts the GCS form. A missing/uncompilable schema or a
  non-JSON value allows the write (graceful degradation); only a value a valid
  schema rejects is refused.

## [0.99.19] - 2026-06-29

### Added

- Compute-node Atlas capture ingest (`AtlasIngest`): turns the keyframe +
  capture-state events a drone forwards to the compute node into a reconstruct
  job — counting keyframes and, on the terminal `Bagged` state, inserting the
  dataset + submitting the job the scheduler picks up. A malformed capture-state
  frame is dropped, never an error.
- Atlas SITL gate harness: in-process, mock-runnable end-to-end gates proving the
  pipeline composes — a simulated capture's events travel drone→compute over the
  real LAN bearer + event router, get ingested, and reconstruct (mock) into a
  splat (G0); a perception offload runs the detector and returns a detection; the
  cluster master aggregates a registered slave. The real-GPU / camera / RF
  criteria stay bench.

## [0.99.18] - 2026-06-29

### Added

- Atlas transport — the two remaining bearers + the WFB-lane receive half:
  - **WfbRelay bearer**: carries small Atlas events drone→ground over the WFB
    radio link (the auxiliary application stream), for the field topology with
    no shared LAN. The lane is decimated (~1.4 KB/datagram), so an oversized
    framed event is rejected with the new non-retriable `PayloadTooLarge`.
  - **Cloud bearer**: publishes the framed envelope to `ados/{id}/atlas/{leaf}`
    over the shared MQTT connection (off-LAN, opt-in), gated on the real
    ConnAck-driven connectivity so the ladder never reaches it against a down
    broker.
  - **GS Atlas relay**: the ground station decodes the drone's `wfb_rx -p 2`
    aux stream and re-POSTs each event into the compute node's receiver, so the
    field lane reaches the same endpoint the direct-LAN bearer uses.

## [0.99.17] - 2026-06-29

### Added

- The control front serves the compute node's cluster status at
  `GET /api/compute/status` (reading the compute heartbeat sidecar), so a
  LAN-paired ground station renders the compute-cluster view local-first,
  fresher than the cloud heartbeat. An absent / stale sidecar is a 404.

## [0.99.16] - 2026-06-29

### Added

- Compute-node install + discovery: a `--profile compute` install now fetches
  the core services (orchestrator, cloud relay, control front, logging, TUI)
  plus the compute daemon, deploys an `ados-compute` service unit that serves
  the job API on the LAN (pairing-gated), and the compute node advertises itself
  over mDNS so it auto-appears in the Add-a-Node card for LAN pairing — like a
  drone or ground station. The compute node folds its heartbeat through the
  existing cloud relay, so it shows up as a fleet node with no new relay code.



### Added

- Plugin compute-offload routing: a plugin's `ctx.compute` calls (register a
  dataset, submit a reconstruct / perception / SLAM job, read status + outputs,
  cancel) now route through the plugin host's capability gate to the paired
  compute node's job API. Each call is gated on its capability
  (`compute.dataset.write` / `compute.job.submit` / `compute.job.read`), and a
  host with no compute node wired returns the not-available shape. Results flow
  on the plugin's own event-bus namespace.

## [0.99.14] - 2026-06-28

### Added

- The cloud heartbeat now carries a generic `pluginState` channel: it reads
  every `<id>-state.json` sidecar under the plugin socket directory, drops any
  that have gone stale, and forwards each slice verbatim under
  `pluginState[<id>]`. Plugins and first-party services own their slice shape
  end-to-end; the heartbeat schema grows no per-plugin fields.
- The world-model capture service publishes its live state (enabled cameras,
  VIO/tracking health, session, keyframes ingested, ingest rate) to its state
  sidecar so it rides the same generic channel.

## [0.99.13] - 2026-06-28

### Fixed

- Install the RustCrypto rustls crypto provider as the process default once via
  a shared `ados_protocol::crypto::ensure_crypto_provider()`, called by every
  bare reqwest client builder (the compute-offload client, the Atlas LAN-HTTP
  bearer, the WHEP poster). Under the workspace's no-provider rustls posture,
  building such a client without it could panic "No provider set"
  non-deterministically under concurrent first builds.

### Added

- Compute-node cloud heartbeat: `ados-compute` writes its cluster + queue state
  to a self-dating sidecar (`/run/ados/compute-heartbeat.json`) every 5 s, and
  the native cloud relay folds the compute fields into the agent heartbeat it
  posts to the cloud — completing the loop for the GCS compute-cluster surface.
  A non-compute node has no sidecar, so the heartbeat is byte-identical to
  before.

### Fixed

- The relay treats a compute sidecar older than its staleness budget as absent,
  so a crashed or hung compute service no longer makes the heartbeat report a
  frozen-but-live cluster state.
- Build the compute-offload HTTP client with the process-default RustCrypto
  rustls provider installed first, so constructing it can no longer panic
  ("No provider set") under the workspace's no-provider rustls posture.

### Added

- Perception / SLAM offload for NPU-less drones (`ados-offload` crate + the
  `ados.sdk.offload` plugin gate): the tier picker (local / offload / hybrid),
  the link-aware freshness gate, and the lock-state safety gate. A drone with no
  on-board accelerator offloads detection / tracking / SLAM to a paired compute
  node and flies its behaviours on the results, while the fast control loop stays
  local. The safety invariant: a result past its freshness budget or a dropped
  link is treated as absent — stop and hold, never extrapolate, never
  auto-re-acquire a dropped lock (only an explicit re-designate re-locks).
  Freshness is anchored on the local monotonic arrival time, so a clock-skewed or
  frozen-but-connected node cannot make a stale stream read fresh.

### Added

- Compute-offload contract: the agent-side `ComputeClient` (a timeout-bounded,
  `X-ADOS-Key`-authenticated client to a paired compute node's job API —
  register a dataset, submit a job with an optional idempotency id, read status
  + outputs, cancel) and the `ctx.compute` plugin facade (cap-gated
  `compute.dataset.write` / `compute.job.submit` / `compute.job.read`), so a
  plugin can run a reconstruction or a perception / SLAM offload on the node.

### Changed

- Harden HTTP client construction in the compute-offload client and the LAN
  bearer: a build failure is now loud rather than silently degrading to a
  timeout-less client.

### Added

- World-model stream lane (`ados-atlas-transport` crate): a bearer-agnostic
  `AtlasBearer` transport over a priority failover ladder (local-first), carrying
  the framed keyframe/delta envelope identically on every bearer. Ships a real
  direct-LAN HTTP bearer (a timeout-bounded sender + a bounded-ingest axum
  receiver that backpressures with 503 and accepts multi-megabyte keyframes), an
  in-process loopback bearer, and a per-device splat-delta WebSocket broadcaster
  (compute -> GCS, lag-skip + idle-disconnect reap + keepalive). The WFB-relay
  and cloud bearers are added with their carriers; the ladder accepts them then.

## [0.99.8] - 2026-06-28

### Added

- Compute-node reconstruct pipeline core. Reconstructor backends (Brush /
  nerfstudio / COLMAP / WebODM) as command-builders behind the `Reconstructor`
  trait, with a PATH-aware selector that falls back to the mock on a node with no
  tool installed; a multi-stage post-flight pipeline (COLMAP poses → splat train)
  that chains each stage with `derived_from` lineage and feeds the prior stage's
  output (`input_uri`) to the next; a live in-flight session state machine
  (pairing → ready → active ↔ paused → ended) with incremental training and an
  SPZ-delta producer behind a trait; and a Rerun-aligned recording adapter that
  maps the keyframe envelope + world-model descriptors onto the Rerun entity-path
  tree (each camera's pinhole logged once). Inert until the compute profile is
  selected.

## [0.99.7] - 2026-06-27

### Added

- Pairing auth on the compute node's job API. A `PairingGate` + axum middleware
  reuse the shared data-plane posture (unpaired ⇒ open, paired + on-box ⇒ open,
  paired + off-box ⇒ `X-ADOS-Key`, constant-time compared), gated uniformly on
  every route including `/api/compute/status`, plus an off-box token-bucket rate
  limiter. With the gate in place the daemon serves with `ConnectInfo` and the
  old loopback-only bind restriction is removed, so a paired node can serve the
  LAN safely (it still defaults to `127.0.0.1`; the installer opts into a LAN
  bind). The pairing-state path honours `ADOS_PAIRING_JSON`, the same override
  the rest of the agent uses.

## [0.99.6] - 2026-06-26

### Added

- The `ados-atlas` capture service: the on-drone world-model capture daemon that
  wraps the capture core. It subscribes the vision engine's frame-descriptor
  broadcast (pixels stay in shared memory), tags each frame with the flight
  controller's fused pose read from the state socket (a local-frame pose: euler
  attitude plus a geodetic-to-local-ENU position) or an offloaded SLAM pose via a
  local / offload / hybrid tier picker, selects keyframes, encodes them to JPEG
  off the reactor, and publishes the keyframe, pose, and capture-state streams on
  a new atlas bus. Camera count is configurable from one to an all-sides rig, one
  flow at any count. Registered as a drone-profile supervised service with its
  own systemd unit; idle and inert until `atlas.enabled` is set (default off).
- A frame-descriptor broadcast on the vision engine (`vision-frames.sock`) so an
  on-box service can subscribe to the descriptor stream and map the ring itself;
  only the small descriptor crosses the socket. A re-stat of the ring file by
  device + inode lets a vision restart be detected so a dead mapping is never
  reused.

### Changed

- The `atlas:` config block is modelled on both the Rust and Python sides with
  matching field optionality and strict enum value sets, so a config valid on one
  half is valid on the other (a minimal camera entry or a partial selection block
  no longer silently disables the service).

## [0.99.5] - 2026-06-26

### Added

- The `ados-atlas` capture core: the light on-drone half of the world-model
  program. A camera config (one camera up to an all-sides rig, one flow at any
  count), a keyframe selector (translation / rotation / time-interval triggers
  with the baseline measured from the last keyframe), and a capture session that
  builds the keyframe envelope, the pose descriptor, and the capture status from
  the shared wire contract. Pure logic, no service wiring yet (inert).

## [0.99.4] - 2026-06-26

### Changed

- Hardened the compute worker for production. The worker now claims a job under
  the engine lock, runs the (real, possibly minutes-long) backend WITHOUT the
  lock, and finalizes under the lock, so a long reconstruction never blocks the
  job API. A cancel that lands during a run now wins (the terminal write refuses
  to overwrite a no-longer-running job). A startup reaper requeues any job left
  in `running` by a crash. The configured worker count now runs that many jobs
  in parallel (each claims a distinct job atomically), and a dedicated task runs
  retention. Backend result metadata (a splat's `gaussian_count`) is surfaced on
  the output record.

## [0.99.3] - 2026-06-26

### Fixed

- Atlas keyframe wire keys now match the contract: the HEVC encoding serializes
  as `hevc-keyframe` (was `hevckeyframe`), the intrinsic and rotation matrices use
  `K`/`R`, and a golden wire-key test pins them so a serde-rename regression fails
  the build.
- A malformed `frame` param on an offload job now fails the job (recorded) instead
  of propagating and orphaning it in `running` (restores the fail-not-stall
  contract). A SLAM offload now records a `pose` artifact, not a `detection`.
- A duplicate job/dataset id returns `409 Conflict` instead of an opaque `500`.
- The compute heartbeat counts jobs with indexed `COUNT(*)` queries instead of
  loading the whole jobs table on every poll; the daemon wires periodic retention
  so the store does not grow without bound.
- The daemon attaches the logging-daemon layer (logs reach the Black Box store),
  refuses a non-loopback bind while the API is unauthenticated, and its doc
  comments no longer claim a unix socket it does not create.

## [0.99.2] - 2026-06-26

### Added

- The compute-node service layer: a native Rust REST job API (axum) for
  datasets, jobs, status, cancel, outputs, and the node heartbeat; the
  `ados-compute` daemon (a worker loop draining the queue plus the LAN job API);
  and a new supervisor `compute` profile that runs `ados-compute` only on a
  compute node. Inert: the `compute` profile is opt-in and no existing node
  selects it.

## [0.99.1] - 2026-06-26

### Added

- The `ados-compute` engine crate: the compute-node core. A SQLite-backed job
  store (datasets / jobs / outputs, FIFO queue, cancel, retention), a scheduler
  with a worker model, the reconstructor and perception-offload traits with mock
  backends (no GPU, no camera, no network), the master/slave cluster view, and a
  node `Engine` with a `tick` and a `heartbeat`. Inert: no service runs it yet.

## [0.99.0] - 2026-06-26

### Added

- An Atlas world-model wire contract in `ados-protocol`: the `atlas.*` and
  `plugin.atlas.*` topics, a tier-aware keyframe envelope (light descriptor vs
  full keyframe, camera id and role, the VIO-vs-offloaded pose source), the
  world-model descriptors, the offloaded-pose return leg, and a compute-offload
  contract (the job interface and the master/slave cluster shape).
- Capabilities `compute.job.submit`, `compute.job.read`, and
  `compute.dataset.write` for pushing work to a compute node. Inert: no service
  consumes the contract yet.

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
