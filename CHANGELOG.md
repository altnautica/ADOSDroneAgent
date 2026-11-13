# Changelog

All notable changes to the ADOS Drone Agent are recorded here.
Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
the project follows [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.109.134] - 2026-11-14

### Fixed

- A go-to the planner finds blocked keeps its goal and path on the pilot frame. The planner holds without a goal or path of its own when no way around is clear, so the frame sent no `goal_ned` and an empty `plan_ned`, and the minimaps and the AR motion layer dropped the go-to's path and goal exactly while it was blocked. The frame now carries the commanded goal and the straight way from the vehicle to it, with `avoid_state` `blocked`, and the minimaps and the AR layer draw both in the blocked colour. The vehicle still holds.
- The AR goal ring takes the blocked colour while the way is blocked, as the path already did.
- The AR motion layer no longer draws a follow-me orbit ring during a go-to. Follow-me publishes its orbit mode and subject while it tracks without flying, so a go-to beside an idle orbit follow drew a ring around the subject the vehicle was not orbiting.

## [0.109.133] - 2026-11-14

### Fixed

- Frame-bus thumbnail video is encoded at its configured bitrate again. Since 0.109.131 the encoder was told the leg ran at the captured rate (20–30 fps) while it received one frame a second, so its rate control spent about a twentieth of the 50 kb/s budget and the thumbnail was heavily blocky. A leg that decimates now declares its own rate on the input, so the bitrate is spent per frame actually sent; gaps are still kept and nothing is duplicated.
- A frame-bus leg set above two thirds of the captured rate (for example 25 fps over a 30 fps camera) now decimates to its rate. The filter keeps one frame per target slot instead of measuring from the last kept frame, which let every frame through above two thirds of the captured rate and halved the rate between one half and two thirds.

## [0.109.132] - 2026-11-13

### Changed

- Obstacle cells on the camera view now end half a metre above the scan plane. The per-camera obstacle grid treated each planar-scan return as a wall reaching 3 m above the vehicle, so cells were drawn well above anything the scan could see.
- The AR motion layer draws a pilot go-to's planned path (`fpath`) and goal (`fgoal`), as it already did for a follow skill.
- Cockpit: the local minimap draws the pilot's planned path (dashed) and a ring on its goal while a go-to or a skill flies, in the go-to state colour, or red while the pilot reports the way blocked and still sends a goal (the goal brake holding on a clear plan). A go-to the planner found blocked sent no path or goal in this release, so the plan disappeared instead of turning red. The plan hides once the pilot frame is stale. The TUI has no minimap, so there is nothing to pair there.

## [0.109.131] - 2026-11-13

### Fixed

- Frame-bus thumbnail video no longer bursts duplicate keyframes after a pause. The thumbnail leg decimated to its 1 fps target with a constant output rate, which, on the wall-clock frame stamps, filled any gap in the frames (a vision-engine or camera outage, or a forward step of the system clock after a time sync) with copies of the last frame and sent them all at once when frames resumed. It now passes the stamps through and keeps one frame per target period in the filter chain, so a gap stays a gap and nothing is duplicated.

## [0.109.130] - 2026-11-13

### Fixed

- Video: an encoder reading a captured camera off the frame bus stamps each frame with the time it arrives instead of frame count at the nominal rate (`-fps_mode passthrough` on a 90 kHz time base; GStreamer `do-timestamp`). A burst, a gap or a source running under its nominal rate now reaches the viewer's RTP clock as it happened, and the stream no longer drifts against wall time. Device cameras keep their output rate cap.
- Video: the frame-bus feed no longer keeps only the newest frame. Frames wait in a four-frame queue and are written at their capture cadence plus a small adaptive hold (at most three frame periods, decaying back to zero while the source is steady), so a burst from the source is spaced back out instead of collapsing to one frame. Frames that never reach the encoder are counted per camera (`frames_written`, `frames_skipped`, `seqlock_misses`, `queue_overflows`, `hold_ms`) in `camera-state.json` and shown as `framebus` in `GET /api/video` and the `video` block of `/api/status/full`.
- Cockpit: the Feed screen holds the drone on the full-rate `hero` profile while it is shown, on every stream quality including Auto (30 s lease renewed every 10 s; a failed renewal is retried within 2 s with backoff; released 3 s after the Feed leaves or the page is hidden). High additionally pins a lease for the whole page, and reloading the page resumes its renewal. The HDMI kiosk runs the same Feed screen. Before, Auto took no lease, so a drone no ground station had promoted streamed its 1 fps thumbnail.
- Dashboard: the video panel takes the same hero hold while it shows a drone's stream and the page is visible. Its fallback tooltips no longer say a freeze moves the feed to HLS.
- Cockpit and dashboard video: a frozen picture redials WHEP only when no bytes arrive either (after 8 s when bytes keep arriving, for a stuck decoder); a freeze never moves the feed to HLS. HLS is used only when ICE fails, and WHEP is retried every 10 s while on HLS (was 60 s). A feed that was playing within the last minute keeps retrying WHEP through an encoder restart instead of stepping down to HLS.
- Cockpit: the main `<video>` element lives once for the Feed screen and is moved between layout slots, so swapping pilot and map views or rotating the screen no longer tears down and redials the stream.
- Cockpit: `?debug=video` shows decoded, shown and received fps, dropped frames, inter-frame time, jitter buffer, bitrate and decoder from `getStats` on the main video.

## [0.109.129] - 2026-11-13

### Fixed

- ADOS Pilot: when Take control is refused (the FC did not answer within 2.5 s, refused GUIDED, or another action answered it `busy`) and the FC still enters GUIDED shortly after, that GUIDED is handled like any GUIDED the pilot did not ask for: under `ados` control the pilot adopts it and flies, otherwise the owner is the FC. Before, the pilot counted it as its own request and did nothing, so the vehicle held in GUIDED with no owner change and no setpoints.

## [0.109.128] - 2026-11-13

### Fixed

- Video: a network camera that stays down is probed every 250 ms only for its first 5 s down, then every 3 s for as long as it stays down, instead of every 250 ms forever. A camera named by hostname is resolved outside the 250 ms connect bound, so a slow resolver no longer fails every probe. A mediamtx that is running but frozen (its API not answering, or an encoder respawn that never gets a packet through) is restarted again instead of only the encoder being respawned.

## [0.109.127] - 2026-11-13

### Fixed

- ADOS Pilot: Take control is answered from the flight controller. Outside GUIDED the pilot asks the FC for it and answers ok, and becomes the owner, only once the FC confirms the mode (an accepting `COMMAND_ACK`, or a HEARTBEAT in GUIDED when the ack is lost). When the FC refuses (for example "Mode change to Guided failed: requires position" during its own failsafe landing) or does not answer within 2.5 s, the action is refused with `fc_refused:<the FC's reason>`, `fc_refused` or `fc_no_answer`, and the owner and control mode stay the FC's. Before, the pilot answered ok and reported itself as owner while the FC kept landing on its own. A second Take control while the FC answers is refused `pending`; Land, RTH, Pause, Kill, Resume, Hand to FC or Fly Here meanwhile ends it `busy`. An action's answer no longer holds up the sticks and heartbeats sent behind it on the same connection.
- Cockpit and dashboard: new refusal texts `refusal.fc_refused`, `refusal.fc_refused_reason` (shows the FC's own words) and `refusal.fc_no_answer`.

## [0.109.126] - 2026-11-13

### Fixed

- Video: when a network camera stops sending, only the encoder is restarted; mediamtx and the WebRTC path stay up. The camera is probed and the encoder respawned the moment it answers again, instead of after a fixed back-off. Stopping the pipeline terminates every stage at once under one short grace, so an encoder blocked on a silent source no longer delays the teardown by several seconds per stage.

## [0.109.125] - 2026-11-13

### Fixed

- Vehicle camera zoom: a stop the camera refuses (`DENIED`, `TEMPORARILY_REJECTED` and the like) shows the zoom failure at once and is still sent again up to three times, like an unanswered stop. Before, a refused stop was dropped silently.

## [0.109.124] - 2026-11-13

### Changed

- Camera zoom route: the zoom request, its deadman and `POST /api/v1/camera/mavlink/zoom` moved into their own module, keeping the camera route file within the size limit. No behaviour change.

## [0.109.123] - 2026-11-13

### Fixed

- Vehicle camera zoom: the cockpit's stop no longer waits for the zoom rate's own answer, which takes up to about 4.6 s when its ACK is lost and longer after an `IN_PROGRESS`, so a tap could drive the lens to its limit. The rate-0 stop goes at the release (a tap still zooms for 0.3 s), and once more if the rate's answer comes after it. A stop the camera did not answer (a `TIMEOUT`, or no reply from the agent) is sent again, up to three sends 250 ms apart. A held zoom also stops when the page hides, the Feed unmounts, the zoom pill goes away or its pointer capture is lost.
- Camera zoom route: a moving continuous zoom (`POST /api/v1/camera/mavlink/zoom`, non-zero rate) is stopped by the agent itself, with rate 0, when no other zoom or keepalive (the same body with `"keepalive": true`, which sends nothing to the camera; 409 when nothing of that component is moving) arrives within 1 s. The cockpit sends a keepalive every 300 ms while a zoom button is held, so a cockpit that vanishes mid-hold no longer leaves the lens zooming.

## [0.109.122] - 2026-11-13

### Fixed

- Pilot: after a Land touchdown, Take control followed by a lift of under 1 m kept the touchdown hold, so Pause was refused and the link-loss and low-battery failsafes did not fire. Any sign of flight now ends the hold: the operator's Take control, an arm or disarm, a take-off, the FC's landed state leaving ON_GROUND, or a climb of 0.3 m seen by the rangefinder or the local position. The failsafes (link loss, low battery, controller loss) and the link-loss countdown follow the FC's own landed state and never wait on the hold, which only refuses Pause, goto, resume and a VTOL transition.
## [0.109.121] - 2026-11-13

### Fixed

- Pilot: the touchdown hold from 0.109.120 could start in the air. It judged the floor by the height used for flying, which falls back to height above home without a fresh in-range rangefinder reading (no rangefinder, past its range, a dropout, or ground below the take-off point), and it started at the hand-over to LAND itself. RTH, Pause, Take control and goto were then refused, and `landed: true` was published, for the whole LAND descent. The hold now starts only on ground evidence: the FC's own ON_GROUND, or, after the pilot put the FC in its land mode, a fresh in-range downward rangefinder reading at or below 0.3 m with the vertical speed under 0.2 m/s for 200 ms; height above home never counts. It refuses only what would command a climb or a hold off the ground (Pause, goto, a ground station's reposition, Resume, a VTOL transition, a skill's motion lease, and a take-off until the FC reports ON_GROUND). RTH, Land, Take control, hand-to-FC and kill are never refused for it, the pilot no longer holds back FC mode changes, and the published `landed` is the FC's own.

## [0.109.120] - 2026-11-13

### Fixed

- Pilot: once a landing touches down the pilot treats the vehicle as on the ground until a take-off, a 1 m climb or a disarm, even while the flight controller has not yet reported it landed (its land detector needs about a second on the ground). In that window Pause, Take control, RTH, Resume, hand-to-FC, a VTOL transition, a goto (from a surface or a ground station's reposition) and a skill's motion lease are refused with `landed`; a take-off is refused with `bad_state` until the FC itself reports the vehicle on the ground; and the pilot never switches the FC out of its land or return mode. The published `landed` follows the same state. Before, a Pause 0.4 s after touchdown switched the FC from LAND to GUIDED, which stopped its land detector, so the vehicle stayed "in the air" for the pilot; a goto 2.5 s later then flew off the ground and a SITL copter tipped over.
- Pilot: Land under `ados` slows its descent toward the hand-over to the FC's LAND (1 m/s², with the FC's response lag allowed for), so the hand-over and the touchdown come at the FC's landing speed: `LAND_SPD_MS`, `LAND_SPEED` (cm/s) on older firmware, else 0.5 m/s. It used to hold 2 m/s down until 0.6 m. In the scripted-FC replay of a 4 m Land with `LAND_SPD_MS` 0.3 the touchdown drops from 1.25 m/s to 0.30 m/s.

## [0.109.119] - 2026-11-13

### Fixed

- Cockpit: a hidden page lets go of the vision detection lease (`hold_s: 0`) and takes it again when it shows, like the view and depth/surfaces leases; a backgrounded tab no longer keeps the drone detecting.
- Cockpit: the flight-state chip keeps the mission ETA when the video stops or the map view takes the screen. While a mission runs or is paused the chip holds its own `ar_route` view lease on the nav stream, merged into the layers' lease, so the agent keeps computing the next waypoint's ETA for it.
- Cockpit and dashboard video: a feed that was live within the last minute restarts every second (no doubling backoff), and a WHEP session that shows no first frame within 3 s is redialled, so the picture returns about a second after the agent's stream does. Measured outage replay: 0.7 s after the stream is back, down from 7.7 s.
- Vehicle camera zoom: ArduPilot's camera library refuses a step zoom, so the cockpit zooms with a continuous rate from the press to its release (a tap zooms for 0.3 s, a lost release stops after 10 s) and the camera zoom route (`POST /api/v1/camera/mavlink/zoom`) takes only `continuous` and `range`. Zoom writes go one at a time so a stop never overtakes its rate, and both zoom types keep the agent's resends.

## [0.109.118] - 2026-11-13

### Fixed

- HDMI kiosk: under `resolution: auto` a 2160-tall ultrawide (5120x2160 21:9, 7680x2160 32:9) keeps its native mode. 0.109.117 still stepped it down to 16:9 1080p; only a panel whose largest mode is 4K and no wider than 2:1 (16:9, or a TV that also lists 17:9 4096x2160) steps down now.

## [0.109.117] - 2026-11-13

### Fixed

- TUI: the flight-state chip words a setup step, an unknown control mode and an unnamed pilot owner in sentence case ("Tuning · Autotune roll"), as the cockpit and dashboard do.
- Cockpit: the aircraft setup offer and the controller tour read the arm state only from a live, fresh telemetry poll; a disarmed snapshot kept from before an unanswered poll counts as unknown (possibly airborne).
- HDMI kiosk: under `resolution: auto` only a 4K panel (a mode of at least 3840x2160) steps down to 1920x1080; QHD and ultrawide panels keep their native mode.

### Changed

- Crates: dropped dependencies the code never used (`anyhow` in ados-hal-probe, `time` in ados-hid, `tower-http` in ados-logd), and ados-swarmbus is a test-only dependency of ados-mavlink-router, so the workspace builds without Cargo's unused-dependency warnings.

## [0.109.116] - 2026-11-13

### Fixed

- Cockpit: the flight-telemetry poll runs for the whole session (5 Hz with the Feed open, 1 Hz elsewhere), so every screen knows the arm state. The aircraft setup offer and the Menu's "Set up aircraft" show again on a known-disarmed vehicle, and leaving a screen with an unsaved draft for the Feed asks first. An unanswered poll still counts as possibly airborne.
- Cockpit: the top bar's link age follows the shared spec format (one decimal below 10 s, whole seconds above).
- Cockpit: declining a cloud unpair while armed names the unpair, not a radio change.
- Dashboard: "Make primary" reads the arm state only from live telemetry; a second camera whose stream fails shows no snapshot rather than the primary camera's; the Logs page keeps the polled list until a stream retry actually opens.
- Router: repeated EXTENDED_SYS_STATE requests on one link-up log at debug, not info.

## [0.109.115] - 2026-11-13

### Fixed

- Follow strip words read in sentence case ("Follow", "Tracking ×", "Searching", "Smart"); abbreviations such as GND, LIDAR, BRG and LAT stay upper case.
- The flight-state chip names a tuning step in sentence case ("Tuning · Autotune roll"), and so does its fallback for an unknown owner or control mode.
- The Skills chip uses the 4 px control radius instead of a pill.
- An HDMI kiosk on a panel wider than 1920 px (a 4K TV) under `resolution: auto` steps the output down to 1920x1080 when the panel offers it, so the cockpit's TV scale reads at its intended size.
- Log store disk probe reads as one expression for the current clippy.

## [0.109.114] - 2026-11-13

### Fixed

- The TUI's flight-state chip words live in their own module, so every Rust file stays within the size ratchet; a camera-match check reads as one expression for the current clippy.

## [0.109.113] - 2026-11-13

### Fixed

- Cockpit: the status pill words a failing pre-flight item as the failure ("Home position not set"), not the passing label.
- Cockpit: the controller tour and the Menu's "Set up aircraft" show only while the vehicle is known disarmed on the ground. An unknown arm state counts as possibly airborne.
- Cockpit: Pause is no longer refused as a failsafe while this cockpit's own Return home or Land waits for its answer.
- Cockpit: going back to the Feed never waits behind the unsaved-changes dialog unless the vehicle is known disarmed on the ground.
- Cockpit: with no fresh pilot state, a controller's Triangle-hold maps to Land, not Take off.
- Cockpit: the Feed's width class comes from its own box beside the menu rail.
- Cockpit: a pose, navigation or pilot report 2000 ms old counts as stale everywhere.
- Cockpit: cloud and radio unpair ask before repeating a refusal with force, and the refusal names the arm state once. "Cancel bind" shows whenever the agent reports a waiting bind. Unknown states, roles and profiles read "Unrecognised". Failed reads on the Pair, Sensors, Mesh, System and Video screens show an error with Retry instead of a made-up state. The Video status tab says "adaptive bitrate", and the camera source picker lists the csi and usb hints.
- Dashboard: the radio-link card names the peer instead of showing a literal placeholder.
- Dashboard: the camera selector switches the video along with its overlays.
- Dashboard: the status pill reads the arm state from a fresh pilot frame and live telemetry only.
- Dashboard: making a gamepad primary is held back while the arm state is unknown, and asks before forcing.
- Dashboard: a refused log stream with a failed poll shows a read error, and the stream retries on a timer.
- Dashboard: the restart banner judges only heartbeats answered after the request, and pauses its timeout while the tab is hidden.
- Dashboard: the unsaved-changes guard covers every settings draft. "Apply suggestion" is disabled when nothing would change. Sensor, uplink and LoRa codes read as words. The navigation drawer recovers from its own load failure.
- Shared: the flight-setup stream reports down and redials as soon as the link goes silent, and starts as down.
- Shared: Fly Here explains a missing flight-controller link instead of staying silent.
- Shared: the armed refusal says "armed or its arm state is unknown".
- Shared: flight-state chips and banners use sentence case ("Link", "Pilot · Normal", "Taking off · Climb", "Controller lost · Holding").
- TUI: detections are polled only while a detector runs, so a slow read cannot hold up the vehicle readings.
- TUI: only a missing gimbal route reads as unavailable.
- TUI: the trend charts hide while telemetry fails or the flight controller is down.
- TUI: the Overview drops old vehicle data when it is reopened.
- TUI: `u` no longer claims "Already up to date" before the installed version is known.
- TUI: a restarting service is not called failed.
- TUI: the Pose row prints its age once.
- TUI: extension state ages are shown only when this clock can measure them.
- The cloud heartbeat's perception tier comes from the same live detector read as `/api/status`.
- `pose_age_ms` grows with the time since the report and is dropped once 2000 ms old.
- The router asks again every 30 s for a missing `EXTENDED_SYS_STATE`, including after the message stops.
- `/api/logs` with a logger filter reports `truncated` when the store window filled.
- The route reference marks every relay-forbidden route by the gate's prefix rule.
- The `pilot` contract is version 2: `armed` and `landed` are null while the flight-controller link is down.
- `ados-swarm` passes the current clippy lints.

## [0.109.112] - 2026-11-13

### Fixed

- Cockpit: switching the Feed to full screen, leaving it, or pressing Escape no longer remounts the Feed, so the stick session stays up (the vehicle no longer brakes to a hold) and a pending Return home or Land keeps its answer. The browser shell check now runs in CI and asserts the Feed survives the toggle.
- Ground-station gamepad reader: an unknown arm or landed state (the pilot cannot hear the flight controller) is treated as possibly airborne: the lightbar never shows the disarmed colour and Triangle-hold maps to Land, not Take off.
- Forgetting a paired Bluetooth device is refused while the vehicle is armed or its arm state is unknown: the cockpit's IO screen holds Forget back and says why, and the agent's armed guard covers `DELETE /api/v1/ground-station/bluetooth/{mac}` (override with `force`).
- Cockpit: the top bar's link cell no longer reads fresh while the flight controller is lost; a pilot frame counts only while it reports the flight controller linked. The telemetry age is read on one clock.
- TUI: current alerts (battery, link, GPS, storage) are no longer dimmed as "(stale)" when the pilot state goes stale.

## [0.109.111] - 2026-11-12

### Added

- `POST /api/wfb/pair/local-bind/cancel` stops a waiting radio bind; the cockpit's Pair screen offers "Cancel bind" while one waits.
- Cockpit: System shows a "Reboot to apply" row while a reboot is pending; the Menu's "Set up aircraft" entry shows while the setup offer stands; Sensors has a Calibration row that opens Aircraft setup.
- Dashboard: a "Rebooting…" / "Restarting…" banner after a reboot or a profile change that restarts the agent, holding back the unreachable banner until it answers again; swarm mode, separation and flocking fields; camera assignments on the Video page.

### Changed

- Cockpit and dashboard show service, radio, GNSS, LoRa, mesh, node, peripheral, sensor, video and capture states as words, with "Unrecognised" for an unknown code; the cockpit status strip, action bar and cloud notice draw their words from the locale file.
- The radio-pairing copy names the right peer per profile ("Pair with a ground station" on a drone); the Controls row says "stick calibration".
- Error toasts no longer start with the HTTP status number, and a refusal while armed reads as one sentence; form check messages name the field in words.
- Dashboard: the Video page hides the radio cards on a visor, a ground station's idle video reads "No stream from the drone yet.", and the cloud relay card names the right node.

## [0.109.110] - 2026-11-12

### Fixed

- The dashboard's mobile navigation drawer loads on its first open, so the dialog code stays out of the main chunk and the main bundle is back under its budget.
- The cockpit browser checks long-press the open part of the small map card for Fly Here; its centre is inside the frame toggle's touch target, so the press used to toggle the frame instead.

## [0.109.109] - 2026-11-12

### Fixed

- The dashboard contrast check lists its source files with `readdirSync`, which the CI's Node 20 has (it used `fs.globSync`, Node 22 only).

## [0.109.108] - 2026-11-12

### Changed

- Browser cockpit: the Feed opens inside the cockpit shell beside the menu rail, with a Full screen button in the top bar (`Exit full screen` to return; the browser's own exit also returns). The HDMI kiosk stays full bleed and has no such button. The dashboard's Cockpit launcher just opens `/cockpit`.
- Cockpit and dashboard chrome follow the sharper, quieter style: smaller radii, near-opaque panels with light blur, hairline borders and no shadows, one control height across the top bar. The flight-state chip is an outline in its state colour with a dot and the second line inline; the status pill is a dot plus text; dashboard badges are outlines with sentence-case text.
- Alert banners sit on a dark panel with only a light severity tint, and severity colours used as small text are lightened to readable values. The heading readout and the left-column captions keep a halo or panel over bright video.
- The take-off line reads "Taking off · Climb"; failing pre-flight items read as their failure ("No home position"), and flight-controller pre-arm items lose the "Flight controller:" prefix.
- `perceptionTier` reports `cpu` when the vision engine runs its detector on the CPU; `ados-offload` 0.2.0 adds `PerceptionTier::Cpu` and the Python SDK's `ResolvedTier.CPU`.
- Unpair, plugin permission grant/revoke and the primary-gamepad write are refused while the vehicle is armed (`?force=1` overrides), and the cockpit and dashboard ask before forcing them.
- The terminal UI's vehicle screen: each follow phase has its own glyph and colour, detection counts come from the latest detections, unknown surface operations read `Other (<op>)`, the navigation-source warning joins the status alerts, pose age shows in the Autonomy block, and an unknown arm state reads as possibly airborne.

### Added

- Cockpit: the pose age in the navigation-source popover; a camera source picker in Settings; an on-screen keyboard for Params and MSP on the kiosk; a passed/total pre-flight count in the Menu; the confirm sheets show each action's message and spec-keyed hints; a refusal toast (no dialog) when Fly Here is pressed while it is not offered.
- Dashboard: a camera selector shared by the detections and layers panels; the pose age on the navigation card; an "Apply suggestion" path after hardware re-detection; the extension failure reason and a readable status; armed-retry confirms on unpair and network writes; a dirty-form guard on settings pages.
- A TypeScript size ratchet (`scripts/check-ts-size.mjs`, 500 lines per file under `cockpit/src`, `dashboard/src` and `web-shared/src`) with a Guards job.
- Shared spec keys for the pilot setup errors, the zoom-off-stage notice, skill parameter steps and the take-off step word.

### Fixed

- Cockpit: the link cell on a ground station reads the age of the newest pilot or telemetry frame instead of always "No link"; the video placeholder says there is no source when the agent's video is stopped; recovery modes count as commanded here only after an accepted (or observed) acknowledgement and never when the flight controller was already in that mode; one recovery-mode set; an acknowledgement line clears only its own text; zoom and frame crop follow the rendered video; the nav status poll waits 2.5 s; `?demo=1` no longer opens the live pilot link alongside the scripted feed; emergency stop may be bound to several controls; the minimap frame toggle, map zoom and altitude-reference toggle take a 48 px press; the compact minimap's north label clears the card title; a minimap opened later keeps the no-fix timer.
- Cockpit: unknown arm state (no flight controller link) shows no tips, marks none seen, offers Land rather than Take off and hides pre-flight pill items; the FC Fly Here route uses the telemetry landed state when the pilot frame is not fresh.
- Shared rules: the FC Fly Here route is not offered while a fresh pilot frame reports a failsafe; AR edge chips keep their plate and text readable when a layer is dimmed.
- Logs: the logger filter matches the logger column on `/api/logs` and `/api/logs/stream`.
- Dashboard: raw codes and enums on the header, mesh, WFB and profile pages read as words; read errors show a retryable card instead of an empty page; Wi-Fi, cellular and uplink switches confirm before dropping the link; parameter writes list what will change; controls meet the 48 px hit target.
- Terminal UI: the vehicle tab no longer drops the pilot and readings panes on short terminals; settings lines truncate instead of wrapping; help and overlays fit the window.

## [0.109.107] - 2026-11-12

### Fixed

- Cockpit browser checks wait for the flight-state chip button, not for the chip text to stop reading "NO PILOT". The spec text is now sentence case ("No pilot"), so the old wait passed at once and on a fast runner a check could tap the placeholder before the first pilot frame arrived (the Mode sheet check at compact and medium widths).

## [0.109.106] - 2026-11-12

### Fixed

- The MAVLink router's connection module is back under the 500-line Rust size limit after the landed-state request field was added.

## [0.109.105] - 2026-11-12

### Changed

- Pilot frame: `armed` and `landed` are `null` while the pilot has not heard the flight controller, instead of repeating the last known state. The router keeps its last "pilot is flying" belief while they are unknown and refuses repositions until the flight controller is heard again.
- `/api/telemetry` reports `landed_state` (`undefined`, `on_ground`, `in_air`, `takeoff`, `landing`) from the flight controller's extended system state, or `null` when it is not reported. On link-up the router asks for that message once a second when nothing else already requests it; a refused or unanswered request is harmless. The vehicle-state wire is now version 6.
- Navigation status (`/api/v1/nav/status` and the nav stream) carries `pose_age_ms`, the age of the vision navigation pose, or `null`.

## [0.109.104] - 2026-11-12

### Changed

- Shared flight-screen spec: a pilot frame is stale from exactly `stale_after_ms` (2000 ms) old on every surface, including the terminal UI. A 1999 ms frame still reads fresh.
- Shared flight-screen spec: the pilot frame's `armed` and `landed` may be unknown (`null`). The status pill hides its pre-flight items, Fly Here never treats the drone as landed, and a lost controller warns, whenever the arm state is unknown.
- Shared flight-screen spec: the Fly Here offer reports why it is refused (`failsafe`, `landed`, `pilot_stale`, `control_mode`, `not_guided`), with new refusal texts "Fly here needs GUIDED on the flight controller" and "Pilot not answering".
- Shared flight-screen spec: a degraded navigation source raises a status pill warning, "Navigation source degraded ({source})".
- Shared flight-screen spec: spec texts for the remaining flight-screen strings (confirm sheet, stop hint, link and readout units, pre-flight icons, terminal UI row labels), and "Pose age {ms} ms" for the navigation source. The unused Medium and Low stream-quality texts are removed.
- Flight-screen chrome tokens are sharper and quieter: radius 6 on panels and 4 on controls, near-opaque panels with lighter blur, a fainter border, and one 36 px control height. The flight-state chip and spec texts read in sentence case ("Mission · WP 3/7", "Active"); acronyms stay upper case. The menu reads "Full screen" and "Exit full screen".
- Shared fonts ship weight 500 for Inter and JetBrains Mono.

## [0.109.103] - 2026-11-10

### Fixed

- Dashboard Home status pill: a drone's storage use now raises the storage warning and critical items. The pill reads storage from the same source as the cockpit (`health.disk_percent`), falling back to `system.disk_pct` on a ground station.

## [0.109.102] - 2026-11-10

### Fixed

- Ground-station panel: only a long press of select arms the emergency stop. On a STOP sheet opened any other way (a short select, touch or a pad), the first long select arms it and only a second one, a second later, fires it.

## [0.109.101] - 2026-11-10

### Fixed

- Ground-station panel: only a long press of select counts toward the two-step emergency stop. A long press of back closes the STOP sheet and never fires the stop, and with STOP focused it keeps its own meaning (the menu).
- Ground-station panel: on the flight screen, previous and next move focus through the flight controls, STOP first, so STOP is one press away from nothing focused; select presses the focused control.
- The panel opens the STOP sheet through the same check as the STOP button.
- Controls editor: Emergency stop is offered only in the Hold cell, so it no longer jumps rows and blocks Save with a conflict.

## [0.109.100] - 2026-11-10

### Fixed

- A ground-station panel reaches the emergency stop again: a long press on the focused STOP opens the STOP sheet with focus on its hold button, and a second long press, once the sheet has been open for 1 s, stops the motors. One long press never stops them.
- Every controller binding of the emergency stop, in the browser and in the ground station's native input, waits for the full 3 s stop hold from the shared layout spec: a press or tap row runs as a 3 s hold, and a shorter hold is raised to 3 s. Stored profiles and direct binding writes are covered, and each raised row is logged.
- Tips stay hidden while the arm state is unknown (flight controller link down, no fresh pilot frame).
- `reposition` reads its return and landing modes from the same shared list as the cockpit, so Copter `AUTO_RTL` and Plane `LOITER_ALT_QLAND` refuse as a failsafe.
- The controller tour card re-places only when a zone mounts, leaves or is renamed, or a zone it avoids changes size, not on every style change.

## [0.109.99] - 2026-11-10

### Fixed

- A controller row bound to the emergency stop always needs the full 3 s stop hold, whatever hold the row or an imported profile sets; the editor and import raise such a row to 3 s.
- A ground-station panel long-press (2 s) no longer completes the 3 s emergency stop hold.
- The cockpit reads the top-level `code` of a refusal and shows its spec text, so a refused reposition reads in plain words.
- PX4 return and landing modes (`AUTO.RTL`, `AUTO.LAND`, `AUTO.PRECLAND`) count as flight-controller recovery modes for the Pause refusal and the controller RTH state; one shared list feeds them.
- `reposition` refuses unless the flight controller is in GUIDED, and refuses during a critical or emergency heartbeat or a pilot-reported failsafe.
- Camera writes wait longer than the agent's own camera budget; an in-progress answer is not a failure and never retries or grabs a frame.
- A step zoom is sent once, never resent.
- The controller tour card re-places when the top bar mounts or resizes.
- With the flight controller link down, the arm state is unknown: the status pill shows Link lost without pre-flight items, and the cockpit treats the drone as possibly airborne.
- The dashboard nav poll times out after 1 s; the terminal UI judges alert staleness on the aged pilot age.

## [0.109.98] - 2026-11-10

### Fixed

- Cockpit and dashboard: the pre-flight "Home position set" check passes as soon as the flight controller reports a home position or its EKF has an origin, so it clears on the ground instead of waiting for the first arm.
- Cockpit: the emergency stop sheet reads "Emergency stop", the kill warning and "Emergency stop · hold 3 s" from the shared texts, counts down while held, and stops the motors when the 3 s hold completes, with no second confirm step.
- Cockpit: the controller cell says "No controller" when no controller is connected.
- Cockpit: the alert list is headed "Alerts", shows each alert's text above its source, and marks the severity with the shared ✕ / ⚠ / ℹ glyphs.
- Cockpit: on the local-frame minimap, "No local position" sits clear of the origin crosshair.

## [0.109.97] - 2026-11-10

### Fixed

- Cockpit: when the controller tour or "Press a control" starts capturing the controller, any held binding gets its release at once and a hold in progress is dropped, so a hold never completes on time spent captured.
- Cockpit: a tip hidden for a moment by a banner, a sheet, a menu or the tour goes back to the head of the queue and shows again after the gap, so the status pill tip is no longer skipped in favour of the flight-state chip tip. Only an auto-dismissed tip waits for the next open.

## [0.109.96] - 2026-11-10

### Fixed

- Cockpit: the controller emergency stop (L2+R2+Circle held) and return home (Options+Circle held) complete with a sheet open. Circle no longer closes the sheet mid-combo, and a completed pad hold sends its action at once.
- Cockpit: a pending choice in the Mode sheet never greys out the flight controller's return, landing or brake modes or the hand-over, and stopping the motion skills no longer waits on a plugin answer first.
- Cockpit: Pause is refused during a flight controller failsafe even when the pilot state is stale: a return or landing this cockpit did not command, or a critical or emergency heartbeat, counts as a failsafe.
- Cockpit: dragging to pick a target maps through the digital zoom; the zoom resets when the video moves into the map card, and a photo saves the zoomed view.
- Cockpit: in the reduced-rate radio fallback each layer keeps its real age, a hung status read times out after 1 s, and a status read in flight is dropped when the stream reopens.
- Cockpit: the left column, Mode sheet and pre-flight checks ignore a pilot state older than 2 s.
- Cockpit: tips never sit over the launch, hand-over or link-loss banners; the controller tour leaves the controller to the safety bindings until the drone is known to be on the ground, and stays below the top bar on short screens.
- Cockpit: Fly Here and camera answers read in words (not supported, not authorized, failsafe), camera requests time out, the camera list refreshes when the flight controller connects, and the Fly Here height stays within the launch ceiling.
- Cockpit: a launch Abort refused from the chip or banner shows its reason; a plugin skill's stop is never held back while its start is pending.
- Command route: `reposition` no longer switches the flight controller to GUIDED. It is refused while a failsafe is active or the flight controller is returning or landing, and the point (0, 0) is refused.
- Camera commands only accept the acknowledgement of the addressed camera, and a photo capture is never resent.
- Vehicle state publishes the heartbeat system status.
- Dashboard: one shared navigation poller; the depth and surfaces toggles start the vision engine; a stale pilot hides the link-loss, height and sticks lines; the status pill reads the flight controller link, not the HTTP answer, and lists failing pre-flight checks.
- Terminal UI: pilot rows dim on the same aged state as the chip, and an old local position no longer feeds the distance.

## [0.109.95] - 2026-11-10

### Changed

- Cockpit: the top-bar status pill shows the critical failsafe item while a fresh pilot state reports the flight controller's own failsafe return or landing.

## [0.109.94] - 2026-11-10

### Changed

- The status pill raises a critical "Failsafe: <cause>" item while a fresh pilot state reports the flight controller's own failsafe return or landing, so it no longer reads "All systems OK" under a failsafe. The shared pill cases cover it, and the dashboard Home card and the terminal UI alert list show it.
- The shared cockpit text gains the confirm-dialog titles and button labels, the local command refusals ("Already armed", "Already disarmed" and the rest), and the remaining HUD, stream, picture-in-picture, quick-settings and instrument words, so every surface can read them from the spec.

## [0.109.93] - 2026-11-10

### Changed

- The follow controls, picture-in-picture swap, vision panel, proximity radar, re-acquire toggle and map status rows now take their words from the shared cockpit text, and an unacknowledged navigation-source switch again says that the flight controller did not acknowledge it.

## [0.109.92] - 2026-11-10

### Changed

- Terminal UI: the Vehicle tab's chip line moves to its own module, so the tab stays within the Rust file size limit.

## [0.109.91] - 2026-11-10

### Changed

- Shared readout cases pin the binary-tie rounding of the V/S and distance readouts (0.35 reads 0.3; 1005 m reads 1.00 km).

## [0.109.90] - 2026-11-10

### Changed

- The cockpit's kiosk alert-column browser check no longer flakes on a slow machine: it now stops the demo layer feed, which also writes a pilot frame, before it waits for "Sticks are with".

## [0.109.89] - 2026-11-10

### Added

- `/api/telemetry` carries `home_distance` (metres): from the GNSS position with at least a 2D fix, else from the EKF local frame against home, so the RTH chip and the bottom row read a distance without GNSS too.
- Terminal UI: the Pilot block shows the operations the pilot offers ("Fly here") and an FC failsafe with its cause in the spec's words; the chip reads the owning skill's detail ("FOLLOW · ORBIT 8 m") from its plugin's echoed parameters.

### Changed

- Cockpit tips: a tip anchored in the top bar sits below the bar, clear of the alert stack; the top bar joins the zones a tip never covers.
- The skill-detail helper is shared by the cockpit, the dashboard and the terminal UI, held to one case file.
- Terminal UI: the Vehicle tab fits a 120×40 terminal: the trend charts give way first, then each block shows what fits and counts the rest.

## [0.109.88] - 2026-11-10

### Changed

- Cockpit spec: one shared set of flight-screen words for the remaining confirm dialogs, quick settings, menu rows, alert titles, pre-flight rows, mode picker, target and lock texts, nav-source chip, proximity radar, local minimap, layer names, video source badges and camera sheet readouts, in sentence case.

## [0.109.87] - 2026-11-10

### Changed

- Cockpit spec: shared words for the follow rail, PiP swap, vision panel, proximity radar, re-acquire toggle, map status rows and the nav-source acknowledgement toast.

## [0.109.86] - 2026-11-10

### Changed

- Formatting only: the flight-controller reposition command and its tests follow rustfmt.

## [0.109.85] - 2026-11-10

### Changed

- Cockpit: "Fly here" on the map also works when a flight controller in GUIDED is armed and flying and the ADOS pilot does not take the point: the same hold sends a reposition to the flight controller, and a refusal reads in the spec's words.
- Agent: `POST /api/command` takes `reposition` with `[lat, lon, alt_m]` (MAVLink `DO_REPOSITION` as a `COMMAND_INT`, height above home within the cockpit's launch range), correlating the flight controller's answer like the other commands.

## [0.109.84] - 2026-11-10

### Changed

- Cockpit status pill: no pilot state, or a stale one, is never reported as OK. The pill also raises an agent that stops answering, a controller lost in the air, and, on the ground, each failing pre-flight check.
- Cockpit flight-state chip: the failsafe and paused-mission lines and the Resume and Abort buttons come from the shared chip rules. Abort cancels a running launch. The chip takes at most 40 % of the bar; when it is cut short, the full text shows on hover or tap. While the nav stream runs at the radio's reduced rate, the chip says so.
- Cockpit top bar: the link cell shows the age of the newest vehicle frame, and its icon turns amber at 3 s and red at 10 s. Tapping the cell shows the RSSI. The battery cell shows the time left to the reserve.
- Cockpit: critical text, the panel and STOP use the spec's colours. Alert-level labels on a TV are at least 16 px. Alert levels and the nav-source popover use the spec's wording. The video placeholder uses the spec's wording and offers Retry when the picture fails or freezes.

## [0.109.83] - 2026-11-10

### Changed

- The terminal UI's vehicle and flight-state code is split into smaller modules to stay within the source size limit. No behaviour change.

## [0.109.82] - 2026-11-10

### Changed

- Cockpit photo and zoom now use the vehicle's own camera over MAVLink when no camera extension takes them: photo goes to a camera that lists image capture, zoom to one that lists basic zoom, and only then to the browser frame grab or digital zoom. A refusal shows "Photo failed" or "Zoom failed" with the vehicle's reason.
- The camera settings sheet has a stream quality choice. Auto keeps the profile the ground station assigned; High holds full-rate video for this window.
- Bottom-row readouts use the shared formatters, so they never show "-0.0", and the distance switches to km from 1000 m.
- The left column shows each action's short label at medium, expanded and TV widths. The full label stays the accessible name.
- The video card in the map view has a button that swaps the video and the map. The follow controls, the follow range panel and the on-screen sticks take their wording from the shared string table.

## [0.109.81] - 2026-11-10

### Changed

- Cockpit: the minimap, the Map panel, the flight instruments, the target overlay and its menu, the what's-locked chip and the Vision panel read their words from the cockpit spec, so they match the other ground surfaces; distances on the map scale, the home distance and the depth ranges use the shared distance format, and the vertical-speed readout uses the shared signed format (never "-0.0").

## [0.109.80] - 2026-11-10

### Changed

- Cockpit: a tap or an early release on a hold control now reads "Keep holding · <s> s" for 1.5 s instead of doing nothing silently.
- Cockpit: the low-battery return-home and link-loss countdowns both show as critical, as the spec sets, and count whole seconds rounded up.
- Cockpit: the banner for a flight-controller mode change names its cause in words ("RC switch or failsafe", "flight controller") rather than the raw code.
- Cockpit: the launch line in the top-centre stack reads the same as the flight-state chip ("TAKING OFF · <step>") and shows exactly while the chip does.
- Cockpit: the Mode sheet warns while obstacle avoidance is off, names what each hold does, and notes that the FC mode list shows the common modes first.
- Cockpit: sheets and their scrim take their top inset from the spec's sheets rule; pilot freshness uses the shared rule (stale from 2 s).

## [0.109.79] - 2026-11-10

### Changed

- Cockpit: a one-time "Set up this aircraft" banner offers the setup wizard for a new flight controller, on the ground and off the flight screen.
- Cockpit: contextual tips wait while a banner or alert is showing on the flight screen and never cover the alert stack.
- Cockpit: the agent-offline banner and the controller sync refusal line use the shared spec strings.

## [0.109.78] - 2026-11-10

### Changed

- The terminal UI Vehicle tab adds a Pilot panel read from the shared cockpit spec: the flight-state chip and its second line, mode, avoidance, mission waypoint, link-loss action or countdown, AGL with its source, and an alert list. The pilot state turns stale after 2 s, as on the cockpit.
- A failed pilot or battery read keeps the last values but dims every row and marks it stale; a kind without an age reads stale. A stale battery route dims the battery block. The GPS panel and the odometry row use the same fix words.

## [0.109.77] - 2026-11-10

### Changed

- Pause is refused with a reason while the flight controller runs its own failsafe return or landing; return home, land and the emergency stop are unaffected.
- Command results with the MAVLink prefix and the unknown landed state now read in the cockpit's own words.
## [0.109.76] - 2026-11-10

### Changed

- The dashboard status pill warns when the pilot has not answered, when the agent is unreachable and when the controller is lost in the air. The fallback refusal text comes from the translation file.

## [0.109.75] - 2026-11-10

### Changed

- The dashboard flight-state chip keeps the last pilot frame when a read fails and turns LINK once it ages, instead of disappearing. It now carries the waypoint ETA, goal and home distance and the skill detail.
- The dashboard Home status card shows the status pill with an expandable alert list, the link-loss action, AGL with its source, who holds the sticks and the time to reserve. Values from a failed or old read show a dash.
- The dashboard polls the pilot state once, 5 times a second while video is shown and once a second otherwise. The video panel adds Depth and Surfaces layer toggles (off by default), a gimbal availability badge, and realigns layers when the stream size arrives.
- Telemetry › Sensors hides the unknown HDOP value. The Enter Cockpit Launch label reads clearly in the light theme.

## [0.109.74] - 2026-11-10

### Changed

- The flight-state chip shows TAKING OFF with the current step and an Abort button while a take-off runs, adds FAILSAFE ahead of the secondary line when the flight controller flies a failsafe return or landing, and offers Resume with the waypoint when the pilot holds a paused mission. A mission ETA of 119.6 s now reads 2:00 instead of 1:60.
- The status pill warns when the pilot has not answered for 2 s and then drops the readiness and flight controller messages from that old state. It also reports an unreachable agent, a lost controller in the air and each failing pre-flight item. Storage now reads like 98 % used.
- Fly here is no longer offered during a flight controller failsafe. The shared rule can also send a reposition straight to a flight controller in GUIDED, for surfaces that use it.
- Video errors (no frames, session lost, transport failed, HLS manifest timeout) now come from the translation file.

## [0.109.73] - 2026-11-10

### Added

- The agent API lists the vehicle's MAVLink cameras and sends image capture and zoom: `GET /api/v1/camera/mavlink` asks every component for `CAMERA_INFORMATION` and answers `{cameras: [{component_id, vendor, model, flags, can_capture_image, can_zoom, ...}]}` (empty when none answer); `POST /api/v1/camera/mavlink/capture {component_id}` sends `MAV_CMD_IMAGE_START_CAPTURE` for one image and `POST /api/v1/camera/mavlink/zoom {component_id, type: step|continuous|range, value}` sends `MAV_CMD_SET_CAMERA_ZOOM`; both answer `{ok, result, component_id}` with the camera's `MAV_RESULT` name or `TIMEOUT`.

## [0.109.72] - 2026-11-10

### Changed

- Pilot state names the cause of a flight controller failsafe; a mission paused by a stick takeover reads paused; an unknown RC RSSI reads unknown.

## [0.109.71] - 2026-11-10

### Changed

- Cockpit spec: flight-screen sheets start below the top bar so STOP stays reachable; the flight-state chip is capped at 40 % of the bar; the link cell shows the newest frame age and the battery names its time to reserve; left-column buttons carry short labels.
- Cockpit spec: the status pill gains sources for a silent pilot, a lost controller in the air and failing pre-flight checks; storage reads "98 % used"; both automatic-manoeuvre countdowns are critical.
- Cockpit spec: tips never cover the alert stack and wait while an alert shows; tip texts fit touch, mouse and pad; shared words for video states, flight controller mode-change causes, failsafe, take-off, hold hints, the setup offer, the nav-source popover, follow, minimap, instruments, targets and the vision panel.
- Cockpit spec: flight-screen panels are more opaque and STOP is a darker red so text keeps 4.5:1 over bright video; tv labels are at least 16 px.

## [0.109.70] - 2026-11-10

### Changed

- Cockpit CI now runs the pad overlay-close, controller editor, pilot stick stream and map gesture browser checks.

## [0.109.69] - 2026-11-10

### Changed

- Cockpit: contextual tips point at flight-screen controls one at a time, never while armed and never over STOP; Settings gains Reset tips.
- Cockpit: the controller tour is a non-modal card driven by the pad that waits until the aircraft is landed, with a replay row in Controls settings.
- Cockpit: shell error banners wrap to two lines instead of clipping.

## [0.109.68] - 2026-11-10

### Changed

- Cockpit spec: the emergency stop built-in is allowed in any arm state (`arm_requirement: any`). STOP is always enabled and always opens the 3 s hold; on a disarmed drone it sends the harmless kill.

## [0.109.67] - 2026-11-10

### Changed

- Cockpit status-pill alert list is laid out on its own panel again on the shared overlay, so it is measured and drawn as before.

## [0.109.66] - 2026-11-10

### Changed

- Cockpit: with a sheet, dialog or the Menu open over the flight screen, the controller drives it (Circle closes it instead of stopping the running skill); pause, return home, arm and emergency stop stay live underneath.

## [0.109.65] - 2026-11-10

### Changed

- Cockpit status-pill alert list and layer menu open on the shared overlay frame: one overlay at a time, Escape, controller back or a press outside closes them, and STOP still takes the first press.

## [0.109.64] - 2026-11-10

### Changed

- Cockpit: every sheet, dialog and menu on the flight screen starts below the top bar and its scrim leaves the top bar clear, so STOP takes the first press with any sheet open; the close button sits inside the sheet header.
- Cockpit: one sheet at a time; Escape or a press outside closes the open sheet before anything else acts, and a press on STOP always reaches STOP.
- Cockpit: the low-battery and link-loss countdowns, the gate ring, the handover banner and "Sticks are with …" clear once the pilot state is older than 2 s.
- Cockpit: a running launch shows its step and an Abort button in the flight overlays, with or without a sheet open.
- Cockpit: a pick in the Mode sheet turns pending at once and its row waits for the answer; results and flight-controller answers read in words; take control, hand to FC and the VTOL transition use the spec's hold times; the fixed-wing avoidance note comes from the spec.
- Cockpit: the Launch sheet takes its altitude range and default from the spec, has no default go-to point, opens prefilled from a map long-press and keeps it across the pre-flight checklist; Launch, Mode and pre-flight words come from the spec.

## [0.109.63] - 2026-11-10

### Changed

- Cockpit: the maps end-to-end check follows the map view with the video in the bottom-left card and exits when it finishes.

## [0.109.62] - 2026-11-10

### Changed

- The follow rail's buttons wait only while the follow skill's own command is out, not while any other command is.

## [0.109.61] - 2026-11-10

### Changed

- Cockpit left column shows a short label under each icon at medium width and up; icons only on compact screens.
- A cockpit flight or camera button that is unavailable now stays pressable and shows why in a toast instead of doing nothing; every column control has a hit area of at least 48 px.
- The bottom row's D shows a distance in GNSS-denied flight, measured from the flight controller's local position origin, and the AGL dims to a dash when the pilot stops reporting.
- Zoom falls back to a digital 1×/2×/4× zoom of the shown video and Photo to a frame grab downloaded in the browser ("Photo saved") when no camera extension offers them; without video the camera buttons say "No video".
- Camera settings open as a sheet over the flight screen (camera choice, recording).
- The gimbal pad keeps one request in flight, backs off on 429, aims with the arrow keys at 30 °/s while focused and recentres on Enter.
- The bottom-left card switches by swipe or dots; each face carries its own title.
- The map view shows the video in the bottom-left card; a picture-in-picture never draws data layers or targets.
- The split divider defaults to the middle, stays between 30 % and 70 %, and is remembered per vehicle with the view.
- The follow range panel no longer repeats the height above ground; the bottom row's AGL is the only one on screen.

## [0.109.60] - 2026-11-10

### Changed

- Cockpit: every map (bottom-left card, full map, split pane, local-frame map) opens Fly Here from a long-press or a right-click; the hold reads its duration from the cockpit spec.
- Cockpit: on the ground the same map gesture opens the Launch sheet with the picked point as its go-to.
- Cockpit: the local-frame map (no GNSS) offers Fly Here, converting the picked point to latitude/longitude through the flight controller's EKF origin.
- Cockpit: the full map's frame label and north mark sit below the top bar; the minimap scale and warning read as one row; map, instrument, vision panel, target chip and re-acquire toggle text are sized in px times the display class scale instead of growing with the viewport; the altitude reference toggle no longer covers the ALT tape title.

## [0.109.59] - 2026-11-10

### Changed

- Cockpit top bar: items move into the Menu in the spec's order (controller, navigation source, link text, link icon, layer button); the chip, status pill, battery, Menu and STOP never move, and the pill truncates with an ellipsis on the narrowest bars.
- Cockpit STOP is always enabled and always opens the 3 s hold-to-confirm ("Emergency stop · hold 3 s"), also while disarmed.
- Cockpit Menu follows the spec's order: moved top-bar items (only those hidden), View, Cameras, Layers, Pre-flight, Flight modes, Controls, Extension panels, Set up aircraft (only when needed), Settings, Exit (none on the kiosk).
- Cockpit camera switching moves from tabs over the video into the Menu, with human labels (role label before a raw device name).
- Cockpit layer button gains a ▾ menu with a checkbox per layer.
- Cockpit top-bar controls have hit areas of at least 48 px; the alert stack, navigation-source chip and no-picture note are sized in px times the class scale, and the no-picture note sits below the centre marks.
- Cockpit status pill keeps readiness and flight-controller texts from a stale pilot frame listed but dimmed; the navigation-source popover shows refusals in words, never a raw result id.
- Cockpit top bar, flight-state chip and Menu words come from the cockpit spec.

## [0.109.58] - 2026-11-10

### Changed

- Cockpit: flight command results and refusals read in plain words from the shared cockpit spec (for example "Return home: Denied", "Resume did not respond", "Still flying; land first") instead of raw result names and codes.

## [0.109.57] - 2026-11-10

### Changed

- Cockpit: the skill picker (GO, Starting…, Stop, ACTIVE, tabs, avoidance chip) and the controller cell read their words from the shared cockpit spec, and the picker marks a running skill in the spec skill colour.

## [0.109.56] - 2026-11-10

### Changed

- Cockpit: the controller Options button opens the Menu, the same as on the other surfaces (it opened the Mode sheet).

## [0.109.55] - 2026-11-10

### Changed

- Cockpit demo mode (`?demo=1`) alternates a mission leg and a follow-me cycle, and the flight-state chip, follow controls and overlays always show the same one.

## [0.109.54] - 2026-11-10

### Added

- The pilot kind carries `setup_step`: the aircraft setup step running while the owner is `tuning` (for example `autotune` or `first_hover`), `null` otherwise, so every surface can fill its `TUNING · <step>` chip. The setup engine reports the running step to the pilot whenever it changes.

### Changed

- The HDMI kiosk keeps its browser profile across reboots in `/var/ados/kiosk/profile` (a desktop-session launch uses `/var/ados/kiosk/profile-<uid>`, owned by that user), so the controller tour, tips, layer level, view and theme it remembers survive a restart. The browser cache stays in tmpfs. Stale `SingletonLock`, `SingletonSocket` and `SingletonCookie` links are removed before every launch so the browser never refuses the profile after a reboot.
- Plugin SDK: the `lease_acquire` docstring lists the `failsafe` refusal. The pilot's goto start is covered by a test for each of its refusals.

## [0.109.53] - 2026-11-10

### Changed

- Cockpit: when the live navigation stream has been down for 5 s (for example over a radio relay that carries only HTTP), the Feed keeps its flight state, follow and map values current by reading the navigation status twice a second, and marks the reduced update rate.

## [0.109.52] - 2026-11-10

### Changed

- Cockpit: detection boxes now dim after 500 ms without a new batch, hide after 2 s, and a "Targets unavailable" chip shows while a detector runs but its boxes have gone stale.

## [0.109.51] - 2026-11-10

### Changed

- Cockpit: a skill press now waits only behind its own unanswered command, so Return home and Land always respond while another command is pending; Return home, Land, Pause and Emergency stop never wait.
- Cockpit: a flight command sent to the flight controller with no answer within 3 s reports "No answer from the drone" instead of hanging.
- Cockpit: a press that is not sent (the skill is unavailable or its previous press is still waiting) shows the reason in a toast.
- Cockpit: Emergency stop is always offered behind its hold and confirm, also while disarmed or with no link.
- Cockpit: Arm and Disarm are checked again at the moment they are sent on every route, so a Disarm confirmed after take-off is not sent and says why.

## [0.109.50] - 2026-11-10

### Changed

- Golden case files for the shared flight-screen rules in `web-shared/src/hud/fixtures/`: `chip-cases.json` (every flight-state chip rule and secondary line, from raw pilot frames), `pill-cases.json`, `refusal-cases.json` (every refusal code) and `tips-cases.json`. The TypeScript tests run every case; other ports can read the same files.

## [0.109.49] - 2026-11-10

### Changed

- Cockpit and dashboard share one implementation of the flight-screen rules in `web-shared/src/hud/`: spec text lookup, refusal texts, the flight-state chip's inputs, the battery reading, the link-loss countdown, the status pill and its base alerts, the "Fly here" offer, and a contextual-tips engine for `tips.json`. Mission Control can copy the same files.
- On-board storage warns at 90 % and turns critical at 97 % (was 85 % and 95 %) in both the status pill and the pre-flight checklist, from the cockpit spec.
- Refusal texts come from the cockpit spec in sentence case ("Pause: No position from the flight controller"); `not_ready:<item>` names the readiness item, and an unknown code reads "Refused: <code>". "Fly here" shows the same texts.
- The link-loss countdown also names `rtl` (Return home) and `loiter` (Hold); an action the spec does not name shows a dash.
- Alert ids match Mission Control's: `link_lost`, `link_stale`, `batt_crit`, `batt_low`, `battery_pack_<pack>_<rule>`, `gps_no_3d`, `video_frozen`, `uplink`; alert titles and details come from the cockpit spec.
- The flight-state chip names the setup step of a tuning flight ("TUNING · <step>") when the pilot reports `setup_step`, and reads "TUNING" without one.
- "Fly here" is no longer offered from a stale pilot state (older than the chip's 2 s).
- The layer engine has a "Targets unavailable" status chip text.

## [0.109.48] - 2026-11-10

### Changed

- Cockpit spec: new `tips.json` describes the flight screen's contextual tips (one at a time, ground only, never over STOP, the left column or the flight-state chip), with their texts in the string table.
- Cockpit spec string table gains the controller tour buttons (Next, Done, Skip, Replay controller tour), one refusal text per pilot refusal code (`refusal.*`, with `Not ready: {item}` and readiness item labels), alert titles (`alert.*`), the no-pilot chip and controller cell texts, the emergency stop hold label, camera texts (digital zoom, photo saved, no video), AGL source marks, MAVLink command result names, and the flight-screen texts of the launch sheet, Mode sheet, menu, Fly here, pre-flight checklist and skill picker.
- Cockpit spec hold-reason labels are keyed by the exact wire string the pilot sends (`controller-lost`, `link-loss`, `obstacle-close`, `blocked`, `lease-busy`, `lease-revoked`) in both `flight-state.json` and the string table; `link-loss` now has a label.
- Cockpit spec layout: top-bar overflow order (controller, navigation source, link text, link icon, layer button), storage warning at 90 % and critical at 97 %, a 20 % low-battery fallback, confirm-hold durations, the map long-press time, launch altitude range and default, menu order, the right column's drop order, a follow strip under the top bar, and the tips pointer. Views: the map view puts its video in the bottom-left card, and the split divider is draggable between 0.3 and 0.7.
- Launch sheet texts read "Go to a point", "Skill" and "Nothing", the words the cockpits already showed.

## [0.109.47] - 2026-11-10

### Fixed

- A skill or the swarm never pulls the flight controller out of a failsafe return or landing: while the pilot reports `fc_failsafe`, a motion lease is refused with `failsafe`, and no goto switches the flight controller to GUIDED. The operator's Take control and the Mode sheet stay available; after Take control a lease is granted again.
- Cockpit shows the pilot's refusals of a built-in action (Pause, Land, RTH, Resume and the others), of Launch and of the launch sheet's Abort in words, for example "Pause: no position from the flight controller" instead of "Pause: no_position". Abort now shows its refusal instead of dropping it.

## [0.109.46] - 2026-11-10

### Fixed

- Cockpit minimap places the drone and offers "Fly here" without a GNSS fix while the flight controller reports a position through its EKF origin, as Mission Control does; the pilot maps the picked point through the same frame. With neither a fix nor an origin the map shows "No GPS fix" and offers no "Fly here".
- Cockpit "Fly here" names a refusal during a flight-controller failsafe return or landing ("change mode first") instead of showing the raw `failsafe` code.

## [0.109.45] - 2026-11-10

### Fixed

- Pilot Pause brakes the vehicle while the flight controller flies on its own, in every control mode: after the pilot hands a ground station's reposition to the flight controller in `fc_manual` or `elrs`, and in any mode the flight controller flies, Pause puts it in GUIDED and holds it as the pilot. Pause answers ok only when it did something: it is refused with `landed` on the ground, `no_position` without a position for the hold, and `failsafe` during a flight-controller failsafe return or landing.
- Pilot tells an operator's return or landing from a flight-controller failsafe: the router reports every ground station mode command toward the flight controller (`SET_MODE`, `DO_SET_MODE`, `NAV_RETURN_TO_LAUNCH`, `NAV_LAND`) to the pilot, and an RTL or land that follows one within 2 s is the operator's. Only an RTL or land nobody commanded (an RC switch, or a radio, battery or EKF failsafe) counts as a failsafe and carries the `rc_or_failsafe` banner; a map-point goto after an operator's Mode-sheet RTL or land flies.
- A ground station's reposition that the router hands to the pilot under ADOS control never pulls the flight controller out of a failsafe return or landing: the pilot ignores it, publishes `fc_failsafe` in its state, and the router answers the sender `TEMPORARILY_REJECTED` instead of `ACCEPTED`.
- A skill or swarm flying through the pilot outside ADOS control yields to a ground station's reposition the flight controller accepted, as the pilot's own hold does: the skill's lease is revoked with `fc-reposition`, and nothing is streamed over the flight controller's target.

## [0.109.44] - 2026-11-10

### Fixed

- Pilot no longer streams its hold over a ground station's reposition in `fc_manual` or `elrs` after Take control: outside ADOS control it gives the flight controller its hold position once and leaves GUIDED's target alone, and when the flight controller accepts a `DO_REPOSITION` the pilot hands the vehicle to it (owner `fc_mode`), so the aircraft flies to the point.
- Pilot refuses a surface map-point goto with `failsafe` while the flight controller flies an RTL, SmartRTL or land the pilot did not command (an RC switch, or a radio, battery or EKF failsafe). The operator changes mode first, with Take control or the Mode sheet.
- Cockpit layer status chips ("Obstacles unavailable", "Depth unscaled") stack in the alert column under the banners and alert cards instead of on the layer canvas, and the handover and "Sticks are with" banners stack under the alert cards instead of over them, so none covers another on the kiosk. The cockpit browser checks cover it. Cockpit bundle rebuilt.

## [0.109.43] - 2026-11-10

### Fixed

- Router and pilot reposition-ACK and goto tests formatted to the workspace rustfmt style.

## [0.109.42] - 2026-11-10

### Fixed

- Router answers a ground station's `DO_REPOSITION` that it hands to the pilot under ADOS control with a `COMMAND_ACK` from the flight controller's id: accepted when the pilot will fly it, temporarily rejected while disarmed, landed or tuning, failed when the point cannot be placed. Surfaces no longer retry into "no answer" while the aircraft flies to the point.
- Pilot no longer takes over a ground station's GUIDED in `fc_manual` or `elrs` control: the flight controller keeps flying the reposition instead of the pilot holding the vehicle where it was.
- Pilot offers and flies a surface map-point goto only under ADOS control; elsewhere it refuses with `control_mode` and lists no `goto` in `surface_ops`.
- Cockpit Fly here is offered only under ADOS control, and its hold sheet says it takes over from any running mission or skill.

## [0.109.41] - 2026-11-10

### Fixed

- Cockpit gimbal pad takes the pointer on press again (except a press on the recentre button), ends a drag when no button is held, and releases on lost capture, so a fast mouse flick off the pad no longer leaves a rate command repeating on a later hover.

### Added

- Cockpit CI runs the browser checks for the first STOP press, the recentre click and the gimbal pad flick-off.
- Pilot tests for the map-point goto refusals while armed on the ground and while tuning, and for a home away from the local origin.

## [0.109.40] - 2026-11-10

### Fixed

- Pilot obstacle-grid cost check runs in the normal test suite: it asserts rays are shared per bearing (at most a third of the cells) instead of an ignored timing test.

## [0.109.39] - 2026-11-10

### Fixed

- Cockpit status pill: the alert list is drawn over the whole window instead of inside the blurred top bar, where it was clipped out of sight and its invisible close layer covered STOP, Menu and the layer button. A press outside the list closes it and still reaches what it hit, so STOP takes the first press with the list open. Opening the list acknowledges only what it shows.
- Cockpit controller tour no longer opens over the flight screen while the vehicle is armed or in the air; it waits until the vehicle is on the ground or another screen is up.
- Cockpit bottom-row AGL marks a height that is not measured above the ground: `HOME` in the warning colour when the pilot kind's value is the EKF height above home, `BARO` when the follow kind's is barometric.
- Cockpit gimbal pad sends at most one rate request per 100 ms while the rate changes (the newest rate, coalesced), resends a held rate before the agent's hold runs out, and sends one zero rate at once on release; a drag no longer posts one request per pointer move.
- Cockpit "Fly here" refuses while the cockpit has no height reading and says why, instead of sending 3 m above home.
- Pilot `goto` is refused with `launching` while an automatic take-off is still climbing, so the take-off's own after-climb action can no longer replace an accepted goto; the cockpit names the refusal.
- Cockpit flight-state chip ages the AR frame's ETA on the monotonic clock only, so a wall-clock step after page load cannot hide or freeze it.
- Cockpit panel buttons and encoder no longer move the hidden menu focus on the flight screen: previous and next do nothing there, and activate opens the Menu.
- Cockpit gimbal pad's recentre button responds to a mouse click: the pad captures the pointer only once a drag leaves the dead zone.
- Cockpit status pill counts a repeated FC warning once, at the worst severity it was sent with, and shows a blocking readiness item as critical.
- Dashboard video panel releases its detection lease while the page is hidden and takes it again when the page shows.
- Pilot obstacle grid casts one ray per half-degree bearing instead of one per cell: rays of one bearing share a march, about 3.5 times less work per camera frame.

## [0.109.38] - 2026-11-10

### Fixed

- Shared layer painter and engine paint without per-frame allocation: dash patterns reused, the staleness check is a bit-packed number, video rects written into reused objects, and status chips pushed by a method instead of a per-paint closure.
- Layer status chips take their colour from the spec tokens (`layer.unavailable_chip`) and their text from the spec strings (`layer.unavailable`, `layer.depth_unscaled`, `layer.ar_approx`).
- Flight-state chip names a hold after a skill's lease ended "Skill stopped" instead of the raw reason code.

### Changed

- Cockpit controller tour opens as a sheet on the first controller connect on any screen (with a "press any button" step while the browser has announced but not reported the pad), not only inside Settings › Controls.

## [0.109.37] - 2026-11-10

### Added

- Pilot `goto` action for ground surfaces: `{lat, lon, alt_m}` with the altitude above home, converted to the local frame from the FC's home position and flown as a pilot goto. Refused with `bad_args`, `not_armed`, `landed`, `tuning` or `no_home`.
- Pilot kind: `surface_ops` (the pilot actions a surface may offer, `goto` today), `agl_m` with `agl_source` (`rangefinder` while a fresh downward reading is under 8 m, else `home` from the EKF), and `statustext`, the FC's STATUSTEXT at WARNING or worse from the last 30 s, newest first, at most eight.
- Cockpit "Fly here": a long-press on the map picks a point, a sheet shows its distance and height, and a hold sends the pilot's `goto`; offered only while the pilot lists `goto` in `surface_ops` and the drone is flying.
- Dashboard status card shows the read-only flight-state chip from the `/api/v1/nav/status` pilot entry, through the cockpit's chip rules (now shared in `web-shared/src/hud/flight-state.ts`).

### Changed

- Cockpit status pill includes the FC's warnings from the pilot kind's `statustext` (critical at ERROR or worse; PreArm lines stay under the pre-arm item).
- Cockpit bottom-row AGL reads the pilot kind's `agl_m`, with the follow kind's height behind it.
- Cockpit spec: the bottom-left card switches by swipe and its page dots; L3 keeps swapping video and map.
- Cockpit controller warning for a non-standard pad reads "Controller layout not supported".

## [0.109.36] - 2026-11-10

### Fixed

- Cockpit top bar measures its overflow in a frame callback (no state set during an effect or ref read during render), the gimbal pad availability no longer resets state inside an effect, and the follow-strip state hook lives in its own module; the cockpit lint passes with no warnings.

## [0.109.35] - 2026-11-10

### Changed

- Cockpit and HDMI kiosk flight screen: the Feed takes the whole viewport. The status strip, the menu rail and the bottom action/hint bar are no longer drawn around it; every other screen is behind the top bar's Menu, and controller button hints show only inside the Menu while a gamepad is connected. Other screens keep their framed shell.
- Cockpit top bar is one row that never scrolls: flight-state chip, status pill, battery (percent, time left, RTL-reserve marker), link, navigation source, controller, layer level, Menu and STOP. When the row runs out of room the controller, navigation source, link text, link icon and layer button move into the Menu in that order; the chip, the pill, battery, Menu and STOP stay, and STOP is fully on screen at every width.
- Cockpit status pill reads "All systems OK", the one active warning or "<n> warnings" from the cockpit alerts, pilot readiness, FC pre-arm texts and storage; a new warning flashes it until its list is opened.
- Cockpit grid zones are placed from one layout model (`lib/flight-layout.ts`): the left and right columns centre between the top bar and the bottom zones, the telemetry row sits between the bottom-left card and the skill picker (raised above them, or distance and height only, where it does not fit), and the instruments draw in the area clear of every zone, centred on the video. No zone overlaps another at any width, glove mode included.
- Cockpit follow controls and the follow range line dock in a strip under the top bar between the columns; the full range panel drops from it on a tap instead of covering the centre.
- Cockpit flight-screen panels use the spec's chrome tokens: translucent blurred panels with a hairline border, a px type scale with tabular numerals, scaled as a whole on a television (kiosk focus rings and the 30 fps overlay cap unchanged). The skill picker chip takes its active colour from the spec's state tokens.

### Added

- Cockpit right column: a zoom pill and a gimbal pad. Dragging the pad sends gimbal rates through `POST /api/v1/gimbal/rate` (resent while held, a zero rate on release), the controller's L2/R2 pitch rate goes through the same sender, and the pad shows only while `GET /api/v1/gimbal/rate` reports the vehicle takes rates. The column drops the pad, then zoom, where the height is short.
- Cockpit test: the flight-screen layout at 1920×1080 (kiosk and browser), 1280×800, 900×600, 760×480 and 580×360, every view, with and without the follow strip, gimbal and glove mode: zones never intersect and stay on screen, STOP stays inside the viewport and the top bar, the instruments stay clear of every zone and draw only at the Instruments level, and the shell draws no legacy chrome around the Feed.

## [0.109.34] - 2026-11-10

### Fixed

- Cockpit detection boxes ease between batches by writing a transform onto the drawn glyphs each frame instead of a React state update per frame, so a moving box re-renders nothing while it eases.
- Cockpit demo mode on a camera node runs the synthetic detector and draws its boxes with no agent status to read.

### Added

- Dashboard video draws the vision engine's detection boxes for the primary camera in the cockpit's corner-bracket style (newest batch at 5 Hz under a detection lease, letterboxed with the video; a stale batch draws nothing).

## [0.109.33] - 2026-11-10

### Fixed

- Cockpit flight-state chip: the mission ETA comes from the next waypoint of the newest AR frame (while it is at most 2 s old), and the GOTO distance is measured from the FC's local position to the pilot goal; both read "—" rather than a made-up value when their inputs are missing.
- Cockpit link-loss countdown names the action the pilot will take (Return home, Hold or Land, from the pilot kind's `link_loss_action`), and the TRANSITION chip follows `vtol_transitioning`.
- Cockpit nav-source chip reads the nine EKF source parameters from one `GET /api/params` instead of nine requests at load.

### Added

- Cockpit minimap draws the mission on the drone (the router's mirror from `GET /api/v1/mission`) while the FC reports a mission, refetched when its id or item count changes.
- Cockpit test: the pilot client frames (input, release, session take, heartbeat, action) match the agent's shared pilot request fixture.

## [0.109.32] - 2026-11-10

### Added

- Pilot nav kind: `link_loss_action` (`rth`, `hold` or `land`, the configured `pilot.link_loss_action`), so a surface's link-loss countdown names the action that will run, and `vtol_transitioning`, true while a VTOL reports a transition to forward or hover flight.
- Readiness: on an ArduPilot older than 4.5, whose MISSION_CURRENT carries no mission state, the pilot adds the warning `mission_inferred` ("FC firmware < 4.5: mission state is inferred") while a mission is on the FC.
- A shared pilot client-frame fixture (`crates/ados-control/tests/fixtures/pilot-requests.json`: input, release, session.take, hb, action) that the pilot stream decodes through its own client path; the operator surfaces check their encoders against it.
- Gimbal rate control for an operator's hold-to-move pad: `POST /api/v1/gimbal/rate {pitch_dps, yaw_dps, hold_ms}` sends `MAV_CMD_DO_GIMBAL_MANAGER_PITCHYAW` with no angle target and the two rates, and sends a zero rate itself when a rate is not renewed within `hold_ms` (100-2000 ms, default 500), so a surface that drops mid-move never leaves the gimbal turning. `GET /api/v1/gimbal/rate` answers `{available, reason}` (a MAVLink flight controller and a fresh gimbal attitude) so a surface shows the pad only where it works.

### Fixed

- Obstacle grid per camera: each cell's ray now goes through the same pinhole and the same vehicle attitude chain as the AR layer (roll and pitch for a body-mounted camera, level for a stabilised gimbal), so obstacle cells sit on the obstacle in the video instead of drifting off it toward the frame edges and when the airframe pitches or rolls.
- Camera model changes (a gimbal tilt or pan) reach the pilot's obstacle projection when they happen instead of up to 2 s later.
- The control front reads the pilot socket from its own task, so a pilot frame half read when the view demand changes is no longer dropped and the pilot and obstacle kinds no longer stall on a misframed stream. The pilot socket is redialled every 2 s.
- Depth and landing-zone frames are kept per camera: a surface leasing one camera gets only that camera's frames, each with its own capture time, so a camera that stops producing is no longer re-sent as fresh whenever another camera produces, and a `?view=` poller always reads its own camera's landing zones. Each depth camera entry carries its own `ts_ms`.
- The detections WebSocket no longer replays a batch older than one second as current when a client connects after detection stopped.
- A detection, depth or landing-zone request for a configured camera that has not delivered its first frame yet (after a vision restart, or while its stream reopens) is held and starts with the first frame, instead of being refused as a camera the node does not capture.
- Changing or resetting the detector (`PUT`/`DELETE /api/vision/detector`) restarts the vision service, so it is refused while the vehicle is armed (`409 E_ARMED`, `force` overrides).
- A vision camera whose stream fails is reopened every 2 s instead of every 500 ms.
- The AR scene holds its 10 Hz (20 Hz while the view moves fast) rate against tick jitter instead of slipping to 7-8 Hz, switches to the fast rate on the first fast tick, and its 10 s build counter is logged even while no vehicle state arrives.
- The operator UI's own static files (bundle chunks, fonts, icons, entry pages) no longer draw on a caller's request budget, so a dashboard opened beside a cockpit on the same laptop no longer fails to load a lazily loaded page on a 429. Data calls stay charged.

## [0.109.31] - 2026-11-10

### Security

- `ados-logd` builds against `rusqlite` 0.41, whose bundled SQLite 3.54 fixes CVE-2026-119375 (uninitialised page-cache memory returned when a database is opened with a sibling `-wal` file of a different page size).

## [0.109.30] - 2026-11-10

### Changed

- Cockpit spec: a fourth layer level, Instruments, above Full (`levels.order` is Instruments, Full, Route, Off). The HUD layer is split into `centre_marks` (the reticle, a short horizon mark and a small heading readout, drawn at every level) and `instruments` (pitch ladder, roll arc, flight-path marker, speed, altitude and heading tapes, drawn only at Instruments), so the default centre is a clean camera view. The level button, `O` and the D-pad step through all four levels and wrap; the back step now goes to the previous level instead of two forward.
- Cockpit spec `layout.json`: the top bar carries STOP, never scrolls and moves the controller, navigation source and link text into the Menu when it runs out of room (the chip, the status pill, battery and STOP never move); a `status_pill` (sources, severity colours, flash until acknowledged); `orientation_lock` for the Android flight screen; `legacy_chrome: "none"` (no status band, telemetry strip, navigation rail or hint bar on the flight screen).
- Cockpit spec `tokens.json`: a `chrome` group for the flight-screen panels (translucent background, backdrop blur with an opaque fallback, hairline border, radius, type scale, icon size and spacing, severity and STOP colours). New strings for the status pill, alerts, levels, battery, Menu, STOP, gimbal pad, map fly-here, the mission on the map and the layer chips.

### Fixed

- Cockpit pre-flight checklist: a "Flight controller checks" item fails while the flight controller reported a PreArm/Arm text in the last 10 s (from the telemetry's `prearm` list) and names the newest one.
- Cockpit: a status-pill model (cockpit alerts, pilot readiness, FC pre-arm texts and storage; "All systems OK", the one alert, or "<n> warnings"; flashing until the list is opened) and on-board storage from `/api/status/full` `health.disk_percent`.
- Cockpit: the follow controls no longer repeat the left column's Hold, Return home and Land.
- Cockpit: depth, surfaces and detection demand leases name a per-window owner, so closing one cockpit window no longer drops another window's lease.
- Cockpit: the kiosk paint cap comes from the spec's `overlay_fps_cap` instead of a second constant; plugin video overlays are layers `plugin:<plugin>:<overlay>` and draw only at Full and Instruments while their flag is on.
- Cockpit: leftover names of the removed skill bar renamed (`Z.zoneChrome`, `Z.topBar`, `BOTTOM_CHROME_CLEARANCE_PX`) and the extensions count string reworded.

## [0.109.29] - 2026-11-08

### Changed

- Shared HUD code: the cockpit layer painter (`paintOps`) moves out of `hud/layer-layout.ts` into its own `hud/layer-paint.ts`, so both files stay within the source size limit and match Mission Control's copies byte for byte. Cockpit and dashboard bundles rebuilt.

## [0.109.28] - 2026-11-08

### Security

- Dependencies: tokio 1.54.1 (`AsyncFd` constructors that broke I/O safety), zstd 0.13.4 with zstd-safe 7.4.0 and zstd-sys 2.1.2 (data from an abandoned stream leaking into the next one on a reused context, `Seekable::decompress` past the end, uninitialized memory on a failed legacy-frame decompress, the WebAssembly allocator shim), and ctutils 0.4.8 (non-constant-time mask generation). The swarm radio registers its receive socket through `AsyncFd::register`. The zip `ZipArchive::extract` symlink advisory is recorded as not applicable: nothing calls the extract APIs, and the plugin archive reader rejects symlink entries before reading any payload.

### Changed

- Plugin host: the flight-control host methods (guided and attitude setpoints, the motion lease, goals and the shaped sticks) are on a `FlightServices` supertrait of `HostServices`. A host implementation adds `impl FlightServices for MyHost {}` and overrides those methods there.
- Source files past the size limit are split into submodules, and the workspace is formatted with the current rustfmt. No change to the agent's behaviour.
- The ONNX backend flattens each session-builder error as it happens instead of carrying the builder through the chain.

## [0.109.27] - 2026-11-08

### Fixed

- Pilot: skill and goto goals now get the stick brake's speed limit. A goal the planner judged clear could still be flown faster than the vehicle can stop: the flight controller flies a position setpoint at the feed-forward plus its pull toward the target, and the planner checks the goal and the way to it, not the stopping distance at the current speed. A follow goal flown at 4.3 m/s toward a tree reached the hard stop at 0.9 m with no room left to stop. What a goal setpoint will ask, and what the vehicle flies now, are held to the same caps as stick flight (obstacles and the space the lidar has seen free, with the flight controller's response time). When either breaks one, the pilot flies the braked velocity from where the vehicle is and reports `detour`, or `blocked` once stopped. As with stick flight, a 12 m lidar keeps goal flight near 6 m/s in open space on the normal profile.

## [0.109.26] - 2026-11-08

### Changed

- Aircraft setup tests: the scripted flight controller now models QuadPlane transitions, the fixed-wing loiter around the GUIDED point and QAUTOTUNE gain saves, and a test flies a tailsitter's whole air wizard to the end. No change to the agent's behaviour.

## [0.109.25] - 2026-11-08

### Fixed

- Aircraft setup on VTOLs: after its transition back to a hover, a VTOL is anywhere on its fixed-wing loiter, tens of metres from where it took off, and the drift check went back to the hover's 20 m box there, so `first_hover` aborted for drift on the way down. Once a VTOL has flown or begun a transition in an air run, the run keeps the plane's 400 m box.
- Aircraft setup: the tuning box is no longer written to the per-aircraft state file, so a box from an earlier session never comes back after a restart; an air step started in flight takes its box from where it starts.

## [0.109.24] - 2026-11-08

### Fixed

- Aircraft setup on VTOLs: the air wizard aborted for drift two seconds after the transition to forward flight. The drift check used the hover's 20 m box whatever the VTOL was doing; it now uses the plane's 400 m box while the flight controller reports forward flight or a transition, and the VTOL's fixed-wing autotune sizes its doublets for that box too.
- Aircraft setup: the tuning box was kept in the per-aircraft state from one session to the next, so an air wizard flown at another field (or after a GPS fix taken before it settled) drift-checked against, and looped around, the old point. Each air run that starts on the ground now takes its box from that take-off.
- Aircraft setup on VTOLs: the `first_hover` climb to the transition height targeted the box centre, and a tailsitter 8 m from it turned around to fly there. The climb now holds the current position.

## [0.109.23] - 2026-11-08

### Fixed

- Aircraft setup on ArduPilot VTOLs: `first_hover` hovered at 3 m indefinitely instead of climbing to 30 m for its transition, because the climb was a local position setpoint, which ArduPlane does not take. The climb now sets the GUIDED target height (DO_REPOSITION plus GUIDED_CHANGE_ALTITUDE; with `Q_GUIDED_MODE` 1 GUIDED is a VTOL hover), re-sends it every 5 s, and lands with `setup.blocker.timeout` if the height is not reached in 60 s.

## [0.109.22] - 2026-11-08

### Fixed

- The depth layer's metres were wrong or missing while flying in a forest. Its scale fit took its references only from the planar LiDAR, whose returns all lie on one image row, mostly on thin trunks the depth model barely resolves: the model read 6.6 m and 10.3 m trunks alike, so the fit either failed (`scale_source: none` for minutes while hovering) or was a guess for the rest of the frame that put most of the ground within 2 m. The downward rangefinder only ever gave one point, below a forward camera's view. The fit now also takes the level ground the rangefinder ranges, with the vehicle's roll and pitch, as references down the lower image (leaving out rays that graze the ground or meet it beyond a LiDAR return on their bearing), and refuses a fit whose agreeing references cover less than a quarter of the frame's value spread rather than extrapolating it. On a recorded forest frame 4 m up the ground cells now read within 1-16 % of their range, row by row.
- A fit carried over to the next frames (the smoothing and the 2 s hold) was applied to that frame's raw model values, although the model rescales its output from frame to frame. The fit is now made on each frame's values normalised by the frame's own spread, so a held or smoothed fit means the same thing in the next frame.
- A downward rangefinder reading at its maximum range (the autopilot's "no return", e.g. 1200 cm of 1200 above 12 m) was taken as a range.
- Cockpit: on a board whose profile declares no vision path but whose vision engine runs (`perception_tier: none` with a configured backend), the Feed never opened the detections stream, so the Vision panel read "No current detections" and no boxes were drawn while the detector ran. The Feed now also opens it when the status services list shows `ados-vision` running.
- Cockpit: the Vision panel's Depth and Surfaces switches read off while the Vision view forced both layers on. While a view forces a layer, its switch now reads on and is disabled with the reason ("On while the Vision view is open"); a second switch keeps the layer's flag for the other views.

## [0.109.21] - 2026-11-08

### Fixed

- A followed subject's lock dropped to `uncertain` after every pitch or yaw swing of the following vehicle, and follow-me held on each. The camera is fixed to the vehicle, so a 12° pitch swing between two 3 Hz detector batches moves the whole image by up to 220 px while the subject itself moves a few tens: the subject's next box landed outside the tracker's gate, the lock coasted and owed a re-confirm frame, and once the subject left the detections the coast held where it had been, so the crop pass looked in the wrong place and the lock was lost. Before each batch the vision engine now moves the camera's track by the vehicle's turn between the previous batch's frame and this one: it keeps four seconds of the autopilot's `ATTITUDE` on the autopilot's clock, reads the attitude at each frame's exposure (by the frame's simulation stamp when the autopilot runs on the simulator's clock, otherwise by the engine's clock through the quickest-arriving sample), and maps the track's centre through the camera's `cameras[]` field of view and mount pitch. Prediction, gate and crop window then sit where the subject now appears. On a recorded follow flight the lock no longer leaves `locked` on any batch the walker was detected on (it did on four), and while the walker was in view but undetected the moved prediction stayed within 50 px of it instead of drifting 190–360 px away. The engine reads the MAVLink socket whenever the tracker is on, not only with a depth model; cameras without a `cameras[]` entry are tracked as before.
- The vision status sidecar's `detection.cameras.<id>` reports `turn_moves`: the detector batches on which the camera's track was moved by the vehicle's turn.

## [0.109.20] - 2026-11-08

### Fixed

- A browser dial of a ticket WebSocket (`/api/v1/nav/stream`, the pilot, aircraft-setup and ground-station streams, `/api/vision/detections/ws`, the Cloudflare log stream) failed its handshake whenever something other than the ticket admitted it: a valid `X-ADOS-Key` on the same request (a header-injecting proxy, a headless browser with extra headers) or the on-box operator. The `101` named no subprotocol although the browser had offered `ados-ws-ticket`, and Chromium drops such a handshake, so the cockpit's data layers read "Depth / Surfaces / Obstacles / AR unavailable" and its detections stayed empty while the agent was producing them. Every admitted handshake that offered the marker now gets it back, whichever credential admitted it; a dial that offered none still gets none.

## [0.109.19] - 2026-11-08

### Fixed

- Aircraft setup on copters: after a completed roll and pitch autotune, "Tune yaw now" started the yaw round on the pack that had just flown roll and pitch. That pack ran into the flight controller's battery failsafe, and the step then kept asking for AUTOTUNE over the failsafe's landing for ten minutes. "Tune yaw now" is now refused (`battery_low`) until the battery is back to 50 % or more, i.e. after a battery swap. Once the flight controller has entered the tune mode and leaves it on its own (a failsafe, a stopped tune), the step fails with `setup.blocker.autotune_failed` (`setup.blocker.disarmed` when it has landed) and no longer commands the tune mode over the flight controller.

## [0.109.18] - 2026-11-08

### Fixed

- Aircraft setup on ArduPilot copters: `autotune` never started. ArduCopter enters AUTOTUNE only from a mode that allows it (STABILIZE, ALT_HOLD, LOITER, POSHOLD) and answered every switch from GUIDED with "Mode change to Autotune failed: init failed", so the step asked again every 2 s for ten minutes. The step now hands the hover to LOITER (QLOITER on a QuadPlane, where QAUTOTUNE then holds position) and enters the tune mode from there; when the flight controller has not taken the hold or tune mode within 10 s the step lands and fails with `setup.blocker.tuning_refused` instead of waiting.
- Aircraft setup on ArduPilot planes: `autotune` was aborted for drifting out of the tuning area about 20 s after it started. ArduPlane's AUTOTUNE flies like FBWA, holding neither height nor position, and the input generator only turned back once 240 m out, so a downwind leg crossed the 400 m area while the plane sank 30 m. The generator now turns back toward the centre whenever it is heading away beyond a quarter of the area (or beyond 40 % in any direction), keeps the turn until the nose points at the centre, and pitches toward the 80 m loiter height outside the pitch doublets.

## [0.109.17] - 2026-11-08

### Fixed

- Aircraft setup: the copter `autotune` step aborted itself as an operator takeover about a second after arming. The landing that ends `first_hover` disarms the flight controller, and the pilot moved its owner from `tuning` to idle on every disarm, so when `autotune` armed again the setup engine saw it had lost `tuning` and took that for the operator taking control. The pilot now keeps the `tuning` owner through a disarm; the setup engine still hands it back when its wizard ends, and an operator safety action (pause, return, land, kill, take control) still ends it at once.
- Aircraft setup on ArduPilot planes: `autotune` never reached its 80 m loiter after `tecs_measure` (`loiter_timeout`). The TECS stages set heights with GUIDED_CHANGE_ALTITUDE, and ArduPlane's GUIDED keeps flying that offboard height and ignores a later DO_REPOSITION's, so the plane stayed at the last stage's 30 m target. The loiter now sends the height as GUIDED_CHANGE_ALTITUDE too.
- Aircraft setup on planes: the TECS idle-glide and dive stages now end the moment the plane sinks below the 50 m floor. They used to run until the settle time had passed, and a dive carried on toward the ground (3 m above it in SITL).

## [0.109.16] - 2026-11-08

### Fixed

- A designated subject the follow camera moved was not re-acquired. Between 3 Hz detector batches a pitch or yaw step moves the whole image by 100–220 px, so a coasting track holding near its last measurement could not reach the subject's next detections through a gate of 1.5 box diagonals. The gate now widens through a coast by `TrackerConfig::coast_gate_growth` (0.5 diagonals per missed frame, up to `max_coast_gate_diag`, 3.0), and every candidate must be within `max_size_ratio` (2.0) of the predicted box's height, so a widened gate takes neither a fragment (a head box, a speck) nor a whole tree.
- A re-confirm frame followed by a miss no longer counts as the re-confirm: the next re-association out of that coast reports `uncertain` for a frame again, as every re-association out of a miss-coast does.
- The crop pass never ran for a subject designated large that then receded. A coasting track holds its last measured box size, and a crop scaled for that size would not magnify past the whole frame, so the crop was skipped for the whole coast while the walking subject shrank below what the whole-frame pass finds. The crop now scales for a subject 5 % smaller per missed frame.

## [0.109.15] - 2026-11-08

### Fixed

- The stick-flight brake let the vehicle reach a column it could not yet see. Pushing full forward pitches a multirotor nose down by about 20°, and a planar lidar 2.5 m up then meets the floor 7–9 m ahead. That return is correctly dropped as ground, but nothing beyond it had been seen, and the brake treated it as free. The column ahead showed in no return until 6 m out, when the vehicle already did 7 m/s, and the drone hit it.
  - The brake now treats the scan's horizon as a possible obstacle: the floor point of each ground-rejected beam, and the lidar's 12 m range for beams that return nothing. The vehicle never goes faster than it can stop within the space it has seen free.
  - Each cap allows for the flight controller's response time, now 0.8 s: the vehicle covers `v·0.8 s` before braking at the profile's acceleration. The SITL traces show the speed still rising for 0.6 s when a cut comes mid-acceleration.
  - The velocity flown is now computed exactly as the one nearest the stick's within every cap, so braking adds no sideways motion.
  - With a 12 m lidar and nothing in sight, stick flight in `brake` mode tops out near 6 m/s (normal profile); `avoid: off` lifts it.

## [0.109.14] - 2026-11-08

### Fixed

- Aircraft setup kept going after the flight controller stopped answering. When a step's reboot ended in a timeout, the next steps "completed" against the cached parameters (a battery apply with nothing to change) or failed later with unrelated blockers (parameter writes that never echo, an autotune take-off that never arms). Every step action except `skip` and `retry` now answers `no_fc` while no heartbeat has come from the flight controller for 3 s.

## [0.109.13] - 2026-11-08

### Fixed

- A designated subject was lost for good after the camera pitched. A follow drone's pitch steps throw every box up to 200 px between detector batches, and the tracker learned each step as the subject's velocity; while it coasted through the following misses it kept extrapolating that velocity, so the prediction ran 40 px a batch away from the subject and the subject's returning detections fell outside the association gate. A coast now damps the center velocity on every missed frame (`TrackerConfig::coast_velocity_decay`, default 0.5), so the prediction holds near the last measurement.
- The tracker could hand a lock to a false box farther from the prediction than the subject. Its motion cue scored an overlapping box by IoU alone and a disjoint box by its distance across the gate, so a narrow subject box overlapping the prediction a little scored below a small false box well off it. The cue is now the larger of the two, so nearer always scores at least as well.

### Added

- The vision status sidecar's `detection.cameras.<id>` reports `crop_runs` and `crop_hits`: the detector batches that also ran on a crop around the designated track, and those on which the crop found a box of the track's class.

## [0.109.12] - 2026-11-08

### Fixed

- Stopping the pilot in flight now fails safe. ArduPilot's GCS failsafe accepts a heartbeat from any component of the GCS system id, and two kept arriving after the pilot died: the router resumed its own heartbeat 1.5 s later, and a vision-navigation component sends one at 1 Hz throughout. A killed pilot therefore left the flight controller in GUIDED. The router now watches the pilot's state stream. If the stream stops for 1 s while the pilot was flying the vehicle (owner `pilot`, `skill`, `goto` or `swarm`, armed, airborne, `ados` mode), the router sets the flight-controller mode for `pilot.link_loss_action`. `rth` sets RTL. `hold` sets Copter or Plane LOITER, or QLOITER on a VTOL. `land` sets Copter LAND, QLAND on a VTOL, or RTL on a plane. The pilot now sends as MAVLink component 196 instead of sharing the router's 191, and the router always sends its own heartbeat.
- The stick-flight obstacle brake missed narrow obstacles and stopped too late:
  - It sampled 13 rays 15° apart, so a 0.6 m column between two rays went unseen. Every occupied cell and scan hit near the vehicle now caps the speed toward it. The commanded velocity is the one nearest the stick's that respects every cap, so braking adds no sideways motion of its own.
  - Distances were measured from the vehicle's current position, while the flight controller's velocity trails a reduced setpoint by about 0.5 s, so the vehicle stopped inside the margin. Distances are now measured from where the current velocity carries the vehicle in 0.5 s.
  - Within 0.6 × the margin, only motion away from the obstacle is kept, and `hold_reason` reads `obstacle-close`.
  - `avoid_state` now reports the brake in stick flight: `detour` while it limits the speed, `blocked` when it stops the vehicle. It previously read `clear` throughout.
  - A stick reversed away from an obstacle responds at once. The brake used to leave the stick shaper's built-up acceleration in place, and it had to unwind first.
- Centred sticks now capture the hold point once the vehicle itself is below 0.2 m/s, commanding a stop until then (for at most 3 s). Previously the point was captured when the commanded speed fell below 0.2 m/s, so a stop from speed overshot the hold point and flew back to it.
- Launch now selects its flight mode before arming: Plane TAKEOFF on a fixed-wing, GUIDED otherwise. It arms once the flight controller reports that mode, and re-sends each command at most once a second. A plane that boots in RTL, where ArduPlane refuses to arm ("RTL mode not armable"), now launches. After 10 s without arming, `sequence.error` reads `mode_refused` or `arm_refused`.

## [0.109.11] - 2026-11-08

### Fixed

- Aircraft setup: the `air` wizard aborted itself about 10 s after the `ground` wizard ended, as soon as the aircraft armed for its first hover. "Use ADOS Pilot" sets the control mode through a pilot action, and that action counted as an operator surface speaking, so the pilot's operator link-loss timer started and the wizard's link-loss abort fired with no surface ever connected. Actions from the setup engine, like those from the plugin host and the router, no longer start the link-loss timer.
- Aircraft setup: a step's blockers are now stored with its state, so `GET /api/v1/fc-setup` still shows why a step failed after its wizard ended (an air abort's trigger read `blockers: []` before), and the engine logs `fc_setup_air_abort` with the trigger.
- Aircraft setup: a failed optional step (airspeed, level, compass, radio and the other skippable steps) now offers `skip` beside `retry`, both plain buttons with no hold-confirm. `retry` is handled by the engine for every step and returns a failed step to its first screen (its instructions and start action, plus `skip` when optional); it was advertised on every failed step but several steps answered it `unknown_action`. `retry` on a step that has not failed answers `not_failed`.
- Aircraft setup on ArduPilot planes: airspeed calibration was always refused. ArduPilot zeroes the airspeed sensor in the ground-pressure calibration (`MAV_CMD_PREFLIGHT_CALIBRATION` p3 = 1, answered IN_PROGRESS then ACCEPTED or FAILED); the p6 = 2 the step sent is not handled there and answered UNSUPPORTED. The step now sends p3 = 1 on ArduPilot (PX4 keeps p6 = 1), reports a FAILED after IN_PROGRESS as a failed calibration rather than a refusal, and is skipped with `no_airspeed_sensor` when the flight controller's SYS_STATUS reports no differential-pressure sensor.
- Aircraft setup on tailsitters: the `frame` step offered only quad, hexa and octa classes and wrote a wing preset over the tailsitter's elevon and throttle outputs, so confirming it changed `Q_FRAME_CLASS` 10 and waited forever for the reboot that change needs. A tailsitter (`Q_TAILSIT_ENABLE`) now keeps its own frame class (10 is offered and accepted) and its outputs (no wing input, no preset written). The `outputs` step tests a dual-motor tailsitter's throttles in ArduPilot's motor-test order (right 1, left 3) with the position question only, since ArduPilot prescribes no spin direction for them.
- Aircraft setup: every step that reboots the flight controller (frame, outputs, battery, compass, filters) gives up after 120 s with `setup.blocker.timeout` instead of waiting forever for a controller that does not come back.
- Aircraft setup on planes: the `frame` step's wing type defaults to the one the outputs already describe (elevon or V-tail functions on any output), and confirming that wing keeps the existing output assignment instead of rewriting it to the preset.

## [0.109.10] - 2026-11-08

### Fixed

- Follow lost a designated person within seconds as they walked away. The person detector runs on the whole frame squeezed into its input, and a person under about 120 px tall in a 720 px frame falls below the size it finds reliably: at the follow standoff a walking subject crosses that size within a second or two of the designation, so the lock coasted and was lost while the subject was in plain view. While an operator-designated track is live, the vision engine now also runs the detector on a crop around the track's predicted box, sized so the subject is about 128 px tall in the model's input (at most 4x magnification), maps those detections back to frame pixels (keeping only whole subjects of the track's class, one box per subject) and feeds them to the tracker with the whole-frame detections. The crop pass runs only while a designated track is live and only when it magnifies the subject beyond the whole frame's own scale, under the same accelerator lease and priority as the detector; it is not counted in the model's reported rate. It needs rgb24 frames, as the re-id crops do.

## [0.109.9] - 2026-11-08

### Fixed

- Follow never locked on a designated subject when the detector missed it on the next frame. An operator designation seeded the engine tracker's track as tentative, like an automatic seed, so one detection frame without the subject (at the 3 Hz detector rate, often the next one) dropped it without a `lost` report, and the following detection seeded an automatic track that never carries a lock state: a follow behaviour waited for a lock that could not come. The designation is now the confirmation: the designated track is confirmed at once, a miss coasts it (`uncertain`), and it ends with an explicit `lost` after the coast budget.
- A coasting track's box no longer shrinks to 1 px. The tracker's box filter learned a size velocity from one smaller detection box and extrapolated it through the coast, which also shrank the association gate (it scales with the box diagonal) so the subject could not be re-associated. The box size is now a random walk that follows its measurements; the center keeps its constant-velocity model.

## [0.109.8] - 2026-11-08

### Fixed

- Aircraft setup on a flight controller that reports no hardware id (ArduPilot SITL, some boards): the wizards never started because AUTOPILOT_VERSION `uid` was 0. The setup state is now filed under `uid2` when it is non-zero (it supersedes `uid`; read from the raw frame, since the agent's MAVLink dialect is built without message extensions), else `uid`, else an id derived from the autopilot, vehicle type, vendor, product, board and firmware version and system id (`d` + 16 hex, a fixed FNV-1a hash, so it is the same after every reboot). `GET /api/v1/fc-setup` and the per-aircraft `state.json` carry `uid_source` (`uid`, `uid2` or `derived`) so surfaces can say the id is derived. A firmware update or a system id change gives a derived-id aircraft a new setup record.

## [0.109.7] - 2026-11-07

### Fixed

- Aircraft setup: every placeholder in a step's text now has its value in the step's `args`, so no surface shows a literal `{n}`. The motor questions carry `n` (the motor number on the layout image; a surface question carries its output channel), plane `level` carries `deg` (the cruise nose-up attitude, 3° when `PTCH_TRIM_DEG` is unset), and `preflight` carries `radius` (the open space to keep clear) and `pct` (the minimum battery).

## [0.109.6] - 2026-11-07

### Fixed

- **Aircraft setup wizard text (dashboard and cockpit).** A step body variant
  the spec does not carry falls back to the step's body, and a bare choice is
  labelled from the spec's `<input>.<value>` text (for example "LiPo" rather
  than "lipo"). Cockpit and dashboard bundles rebuilt.

## [0.109.5] - 2026-11-07

### Fixed

- The pilot never received the mission state. It now reads it from the router's state socket. A launch sequence that started the mission runs its "when done" (Land or RTH) once, when the mission reaches `complete` while airborne. `resume` continues any mission the router reports paused, including one stopped by a hold mode: with DO_PAUSE_CONTINUE in AUTO, otherwise by putting the flight controller back in AUTO at the item it was on. It refuses once the mission is complete or gone. The `mission` of the pilot state now comes from the pilot, and the control front no longer copies it in.
- `land` while the flight controller is flying (a mission, or a mode of its own) now sets its LAND mode. Before, it waited for a pilot descent that never ran.
- The pilot's `mission` object accepts null `seq`, `total` and `age_ms`, as the router publishes them before the first MISSION_CURRENT.

## [0.109.4] - 2026-11-07

### Added

- Aircraft setup routes, served by the pilot's setup engine: `GET /api/v1/fc-setup` (identity, firmware, vehicle class, the running step, both wizards with every step, and the setup report with its parameter snapshots), `POST /api/v1/fc-setup/{wizard}/start`, `POST /api/v1/fc-setup/{wizard}/abort`, `POST /api/v1/fc-setup/{wizard}/steps/{step}/run` (one step on its own), `POST /api/v1/fc-setup/{wizard}/steps/{step}/actions/{action}` (`{"args": {…}}`), `POST /api/v1/fc-setup/revert` (`{"snapshot"}`, default the setup snapshot), and `WS /api/v1/fc-setup/stream` (ticket scope `fc.setup`: step frames at 5 Hz while a wizard runs, wizard frames on change, a keep-alive each second). Engine refusals answer `{"ok": false, "err"}` with 422 (bad request), 409 (state), 404 (no snapshot) or 503 (pilot not running). MCP tokens cannot reach the write routes.

## [0.109.3] - 2026-11-07

### Added

- `WS /api/v1/pilot/stream` (ticket scope `pilot.control`, flight class, never minted for an MCP token): an operator surface's sticks (`input`), `release`, `session.take`, `hb` and named actions go to the ADOS pilot; the surface receives the pilot state as `{kind:"pilot", ts_ms, data}` at 10 Hz, `{kind:"action_result", name, id, ok, err}` per action and a 1 s keep-alive.
- The nav stream and `GET /api/v1/nav/status` carry the pilot's `pilot` kind (with the router's mission state) and `obstacles` kind; the nav hub's view demand, with each camera's projection model, goes to the pilot so it projects obstacles only for watched cameras.
- `POST /api/command` in `ados` control mode routes arm, disarm, takeoff, land, rtl, pauseMission, resumeMission and killSwitch through the pilot's actions and their readiness checks.
- Aux channel `Pilot` (10): a ground station serves the drone pilot's socket, sends operator traffic up the aux uplink unbatched, and receives the pilot's state about twice a second from the drone; the drone forwards uplinked pilot traffic addressed to its flight controller to its pilot.

## [0.109.2] - 2026-11-07

### Added

- The installer deploys `ados-pilot` on the drone profile from the release asset `ados-pilot-aarch64`, which the release workflow now builds. The binary is best-effort: an install without it keeps the router's own heartbeat and passes client stick frames through, as before.

## [0.109.1] - 2026-11-07

### Added

- Aircraft setup on the agent: `ados-pilot` hosts a flight-controller setup and tuning engine with two wizards that every surface renders from one step schema (title and body keys, image, live values, actions with their confirm style, blockers, result). `ground` (stationary, no propellers spinning): `detect` (firmware, version, vehicle class, full parameter table), `frame` (copter and VTOL frame class and type, plane wing type outputs, PX4 airframe; prop size, motor KV, cells), `accel` (six poses through ACCELCAL_VEHICLE_POS, PX4 `[cal]`), `level`, `gyro` (PX4), `compass` (onboard calibration with a fitness limit of 16 mGauss, up to three attempts, known-heading option), `airspeed`, `radio`, `rc_ados_switch` (one flight-mode switch position set to GUIDED and labelled ADOS), `outputs` (motor order and direction one motor at a time behind a propellers-removed hold, a DShot direction fix in `SERVO_BLH_RVMASK`, control-surface directions with `SERVOn_REVERSED`), `battery` (capacity, cells from voltage, voltage multiplier from a multimeter reading), `initial_params` (the initial tune from prop size and cells; plane airspeeds and TECS defaults), `notch_static` (ESC-telemetry harmonic notch), `ados_profile` (the FC failsafe parameters ADOS Pilot depends on, the same table as the pilot's readiness check) and `ados_intro` (choose ADOS Pilot or FC control). `air` (flown by the pilot as owner `tuning`, the operator able to take over at any time): `preflight`, `first_hover` / `first_flight` (vibration, learned hover throttle, attitude tracking, rate oscillation), `notch_air`, `trim_air`, `airspeed_cal`, `tecs_measure`, `autotune` (Copter AUTOTUNE and QAUTOTUNE with the pilot as the RC proxy, roll and pitch then yaw, resumable after a battery swap; Plane AUTOTUNE with generated roll and pitch doublets inside the tuning box; PX4 MAV_CMD_DO_AUTOTUNE_ENABLE) and `verify` (tuned flight against the first flight, with "Revert gains"). Air steps abort to a landing (or a return, fixed-wing) on operator sticks, a pilot action, link loss, battery under 30 %, wind over 8 m/s or drift out of the box. State persists per flight controller under `/var/ados/fc-setup/<uid>/`, with a full parameter snapshot before the first change and a revert.
- The readiness item `setup_needed` blocks while this aircraft has no completed `ground` wizard or the flight controller reports a calibration PreArm message.

## [0.109.0] - 2026-11-07

### Added

- ADOS Pilot (`ados-pilot`, drone profile, unit `ados-pilot.service`): the on-board command authority. While `control.mode` is `ados` it is the only producer of motion setpoints and the flight controller flies GUIDED, keeping stabilisation, the EKF, mixing and its own failsafe. It shapes operator sticks with the Cine / Normal / Sport profiles (`pilot.profile`): velocity with acceleration and jerk limits, position, altitude and heading hold when centred, the height reference from the downward rangefinder below 8 m (`pilot.height_ref`), a 0.5 m floor with landing protection, the `pilot.max_alt_agl_m` ceiling and the `pilot.max_dist_m` geofence. Fixed-wing and VTOL forward flight map the sticks to heading rate, climb rate and airspeed, sent as guided change commands.
- The pilot arbitrates every motion source through one owner (`idle`, `pilot`, `skill`, `goto`, `swarm`, `mission_fc`, `fc_mode`, `tuning`). It yields to FC missions and FC modes, takes over on a stick deflection during a mission, adopts an external switch into GUIDED, and grants skills a single motion lease. A held stick (above 0.9 for 1 s), pause, land, return-home, a control-mode change, an FC mode change or one second without a goal revokes the lease.
- Obstacles: the pilot builds a 128 × 128 clearance grid at 0.25 m from the live occupancy tiles and the levelled planar scan, with ground returns dropped. Stick flight brakes toward obstacles and slides along them (`pilot.avoid` `brake`). Skill and goto goals go through a planner that keeps a clear goal, otherwise detours to the cheapest reachable candidate that still sees the subject (A* over the grid), or holds when blocked. A return closer than 0.6 × the margin always holds. `pilot.obstacle_margin_m` defaults to 1.5 m.
- Failsafes: controller loss (no stick frame for 300 ms) brakes to a hold; no operator surface for `pilot.link_loss_s` (10 s) runs `pilot.link_loss_action` (`rth`, `hold` or `land`); a low battery starts a cancellable 10 s return countdown. The pilot sends the 2 Hz heartbeat the FC's GCS failsafe watches, so a stopped pilot makes the FC fail safe.
- Flows: arm, take off (`alt_m`, default 1.5 m), the one-action `launch` sequence (checks, arm, take off, then hold, a skill, the mission or a goto, with an optional land or return when done), land, return home, pause, resume, take control, hand to the FC, kill, and the profile, control-mode, avoidance and VTOL transition actions. Human-initiated handovers pass a centred-sticks gate; one stick session is accepted at a time, transferred with `session.take`; handovers raise a banner.
- Readiness items for the pre-flight checklist, including `fc_profile`: the flight controller's failsafe parameters (`MAV_GCS_SYSID`, `FS_GCS_ENABLE` or `FS_GCS_ENABL`, the GCS timeouts, `GUID_TIMEOUT`, `RC_OVERRIDE_TIME`, the battery failsafe actions) must match before arming in `ados` mode.
- The `pilot.v1` socket (`/run/ados/pilot.sock`) publishes `state` (the nav kind `pilot`) and `obstacles` (72 clearance sectors, plus per-camera grids only for cameras a viewer leased with the obstacles layer).
- Plugin methods `flight.lease.acquire`, `flight.lease.release`, `flight.goal.send` and `flight.sticks.subscribe` under the new capability `flight.motion`, with the host events `flight.sticks` and `flight.lease.revoked` delivered only to the lease holder. A stopped plugin's lease is released. The Python SDK gains `ctx.flight.lease_acquire`, `lease_release`, `goal_send` and `sticks_subscribe`.
- Config: `control.mode` (`ados`, `fc_manual` or `elrs`; default `ados`), `control.rc_ados_position`, and the `pilot` block.

### Changed

- With the pilot alive in `ados` mode, the router drops client MANUAL_CONTROL and RC_CHANNELS_OVERRIDE, redirects client gotos (DO_REPOSITION and position SET_POSITION_TARGET) to the pilot, stops its own heartbeat, and hands the swarm loop's velocities to the pilot. In `fc_manual` and `elrs`, or with the pilot stopped, nothing changes.
- `flight.guided_setpoint.send` from older plugins becomes a position goal under an implicit motion lease while the pilot runs, so those setpoints also pass the pilot's limits and planner.

## [0.108.20] - 2026-11-07

### Fixed

- The mission mirror also fetches the plan when the vehicle enters a mission mode with nothing mirrored, and when MISSION_CURRENT reports a seq past the mirror's last item. A plan uploaded over a link the router cannot see, on firmware whose MISSION_CURRENT carries no `total` (ArduPilot before 4.5), left the mirror empty and the mission state `none`.

## [0.108.19] - 2026-11-07

### Added

- Mission state, computed once on the agent. The vehicle-state snapshot (`state.v2` body version 5) carries `mission`: `state` (`none`, `loaded`, `active`, `paused`, `complete` or `returning`), `seq`, `total`, `item_reached`, `id`, `source` (`mission_state` when the flight controller reports it, `fallback` on firmware that does not, such as ArduPilot before 4.5), `age_ms`, `landing_seq` (the AUTO_RTL landing sequence), plus the mirror's `rev`, its navigation items and `uploads_seen` (uploads by any ground station through the router that the flight controller accepted). It is derived from numeric flight modes, MISSION_CURRENT with its extension fields, MISSION_ITEM_REACHED, DO_PAUSE_CONTINUE and its acknowledgement, never from mode names. Copter, Plane and PX4 mode numbers are covered.
- The router keeps a mirror of the flight controller's main mission. It captures a ground station's download or upload passing through it, and otherwise downloads the mission itself at the first heartbeat, after an upload it could not capture, and when MISSION_CURRENT's `total` disagrees with it. A download starts only after 3 s without another component transferring, stops the moment one starts, retries each reply three times after 1 s and backs off 2 to 30 s after a failure.
- `GET /api/v1/mission`: the mirrored mission, every MISSION_ITEM_INT field in field order, with the live mission state. The plugin method `mission.read` returns the same mirror; the `mission.read` capability can now be granted.
- The snapshot also carries `home` (HOME_POSITION in both frames), `gimbal` (gimbal pitch and yaw from GIMBAL_DEVICE_ATTITUDE_STATUS, or MOUNT_ORIENTATION when that is absent, with an earth-frame flag and age), `prearm` (PreArm and Arm messages from the last 10 s) and `attitude.time_boot_ms`.
- The AR scene: the nav stream kind `ar` projects the mission route (next, after and other waypoints, the active, next and future legs), home, edge arrows for an off-screen next waypoint or home, the flight-path vector and the follow path, goal and orbit into each camera, normalised to the image, every item keyed by `k`. The route shows while a mission is active or paused, and as a preview while loaded on the ground. Configured by `ar.enabled` (default true) and `ar.rate_hz` (default 10).
- View leases on `WS /api/v1/nav/stream`: a client sends `{"type":"view","camera_id","layers":[…]}` (layers `ar_route`, `ar_motion`, `obstacles`, `depth`, `surfaces`) and re-sends it every 2 s; a polling client passes `?view=<camera_id>:<layer,…>` on `GET /api/v1/nav/status`. Overlays are computed only for leased cameras and layers, once per camera however many surfaces watch it, at 10 Hz rising to 20 Hz while the view moves fast; an unchanged frame is not resent, only a 1 Hz keep-alive. Each client receives only its leased camera. `GET /api/v1/nav/status` reports the union as `view_demand`. The nav stream gains the kinds `pilot`, `obstacles`, `depth` and `surfaces` for host producers.
- The control plane serves the vision engine's `vision-depth.sock` records as the nav kinds `depth` (`{v, ts_ms, scale_source, cameras:{<id>:{cols, rows, scale_source, near, measured}}}`) and `surfaces` (`{v, ts_ms, camera_id, zones:[{pts, score, slope_deg}]}`).

### Changed

- The drone stream table requests LOCAL_POSITION_NED at 50 Hz, and adds EXTENDED_SYS_STATE (2 Hz), HOME_POSITION (every 5 s), GIMBAL_DEVICE_ATTITUDE_STATUS (10 Hz) and MISSION_CURRENT (2 Hz).
- The state socket's body version is 5. Every reader in this release reads it; a reader from an older release skips the frames until it is upgraded too.

## [0.108.18] - 2026-11-07

### Added

- Ground station: native controller flight. `ados-input` reads the primary pad (DualShock 4 over USB or Bluetooth, or any standard pad) at 50 Hz and, in control mode `ados`, streams pilot `input` frames (src `gs-native`, Mode 2 default, radial deadzone 0.08) to the ground station's pilot relay socket `/run/ados/pilot.sock`. A lost pad, a failed poll, a dropped relay or a mode other than `ados` sends `release`. Both DualShock 4 kernel maps are read: `hid-playstation`, and the older `hid-sony` map (right stick on `ABS_Z/RZ`, triggers on `ABS_HAT2*`) for a pad without `ABS_RX`. The map, gestures, colours and patterns come from the cockpit spec `ds4.json`, compiled in.
- Ground station: the safety buttons run natively in every control mode. PS brakes the moment it goes down; held on for 2 s it returns home (rumble countdown); PS during RTH cancels to a hold; Options + Circle is the PS fallback (same press and hold); L1 + R1 held 2 s with the left stick centred arms; L2 + R2 + Circle held 3 s kills. L2 + R2 + Share held 2 s is the emergency ELRS lane: it sends `control_mode:elrs` to the drone and switches the station's CRSF lane at once, latched until the drone reports `elrs`.
- Ground station: the DualShock 4 lightbar (multicolour LED class) and rumble (`FF_RUMBLE`) follow the drone's pilot state: disarmed green, armed idle white, pilot blue, skill cyan, mission magenta, RTH amber blinking at 1 Hz, low-battery RTH red pulsing at 0.5 Hz, failsafe or lost link red blinking at 4 Hz; one short rumble on an owner or skill change, two on a warning, three long on a critical. The battery level is read from `ps-controller-battery-*`.
- `GET /api/v1/ground-station/input`: `native_sticks` (the kiosk cockpit then ignores the sticks and the safety buttons), `pilot_relay`, `streaming`, `control_mode` with `control_mode_source`, `controller {id, name, connection, driver, battery_pct}`, and the effective binding document. Read from the new runtime file `/run/ados/gs-input-state.json`.
- `PUT /api/v1/ground-station/input/bindings` takes the Settings › Controls document. A document without a locked binding (pause press, RTH hold, arm combo, kill combo; each on pressable buttons, held at least as long as the default and under the default's condition) is refused 422 `E_LOCKED_BINDINGS_MISSING` with the missing list; an accepted one applies at once and is saved in `/etc/ados/ground-station-input.json` next to `primary`.

### Changed

- Ground station CRSF lane: in control modes `ados` and `fc_manual` the operator's pad no longer flies over ELRS. The lane sends a neutral set (sticks and throttle centred, aux channels idle) at its frame rate, so the ELRS link and the receiver stay up as the backup lane and the FC sees no RC failsafe; a live programmatic injection still flies under inject authority. Control mode `elrs` keeps the previous behaviour. With no ground-station input state file the mode defaults to `ados`. The lane reports `channel_source: "neutral"` while it sends the neutral set.

||||||| Stash base
## [0.108.17] - 2026-11-07

### Fixed

- Cockpit: the controller stick stream judges a pad live by the poller reading it, not by `Gamepad.timestamp`. Browsers advance that timestamp only when a value changes, so a stick held still (centred, or pinned at full) read as lost after 150 ms, sent `release` and re-armed the centre-sticks gate. The stream also releases while another screen or the quick menu holds the controller, so menu navigation on the left stick never reaches the throttle.
- Cockpit: Pause and Emergency stop are never held behind another command still waiting for its acknowledgement (Land then Pause brakes at once), on the left column, Space, PS and the follow rail.
- Cockpit: PS brakes the moment it goes down; held 2 s it escalates to RTH. Options + Circle, the fallback where the system swallows PS, brakes on press and reaches RTH after a 2 s hold. The controller spec gains the `press` gesture for these rows.
- Cockpit: a locked controller binding can move only to a button (never a stick or the lightbar), and a binding file counts a locked action only with its default hold time and condition (arm: 2 s with the left stick centred; kill: 3 s). An import is applied whole or refused, so its stick mode and deadzone never go live ahead of its bindings.
- Cockpit: a pad the browser does not report in the standard layout drives nothing: the top bar shows "PAD · NOT STANDARD". The unused non-standard axis-map helpers are removed; such pads are not supported.
- Cockpit: F1–F3 select the Cine, Normal and Sport profiles again.
- Cockpit: the skill picker's GO checks the arm, link and freshness state of the moment, not the state when the picker opened.
- Cockpit: a stalled pilot (state older than 2 s) hands RTH, Land and Kill to `POST /api/command` while the FC link is live.
- Cockpit: passing the pre-flight checklist from the Launch sheet returns to the Launch sheet, so its step strip, Abort and any refusal stay in view.
- Cockpit: a plugin skill for another vehicle class never fires from a key, a controller binding or the Launch sheet.
- Cockpit: the GOTO chip shows "—" for the distance; the pilot state carries no vehicle position and the old value read "0 m" on every clear path.
- Cockpit: the Vision view's detector picker lists detection models only, is disabled while armed (switching restarts the vision engine), and its second-tap confirm expires after 3 s. The depth layer's range (Off, 2 m, 5 m) and style (Solid, Outline) are set there and persist per vehicle.
- Cockpit and dashboard: each data layer (depth, landing zones, obstacles) dims and hides on its own clock, so a dead depth stream never rides on fresh obstacles, nor stale obstacle cells on fresh depth. A repeated frame (the agent's keep-alive on a still scene) refreshes only the layer's freshness, so the dashboard's route no longer hides after 2 s on a still scene and items never snap back once a second. Items new after a still period fade in instead of popping.
- On a ground-station kiosk whose input status is not known yet, the browser leaves arm, kill, pause and RTH to the native input.
- TUI: the Owner row no longer repeats the skill name.

## [0.108.16] - 2026-11-07

### Added

- Depth and landing-zone layers. The vision engine runs a monocular depth model (MiDaS v2.1 small, registry id `depth_midas_v21_small`) on a camera only while a consumer holds a `depth` or `surfaces` lease there, shared with the detection lease map: `vision.request_detection` and `POST /api/vision/demand` take an optional `kind` (`detection`, the default, `depth` or `surfaces`), and the reply names it. The model runs at 2 Hz on two ONNX threads on the CPU runtime, or 5 Hz on the Rockchip accelerator through the sidecar's new `dense` request, and only when no detector or re-id inference is waiting for the accelerator. Each run pools the map to a 32 x 18 grid (each cell's nearest tenth) and fits its scale and shift to the planar LiDAR returns and the downward rangefinder spot that land in the image (median pairwise slope, at least 8 points, smoothed with a 0.3 moving average held for 2 s); without a fit the grid is relative bands only and says `scale_source: "none"`. While surfaces are leased, the scaled map is cast into the levelled body frame and searched for 2 m ground bins with slope at most 10° and plane residual at most 0.1 m, near the rangefinder ground; joined bins become zones scored by area, at most once a second.
- `vision-depth.sock`: the engine's depth grids and landing zones as length-prefixed msgpack records (`ados_protocol::vision_depth`).
- `vision.depth` config (`enabled`, `model_id`, `model_path`, `input_width`, `input_height`, `rate_hz`, `threads`). Every key is optional: with no block the engine loads the default model file from `vision.models_dir` when it is installed. The model's camera geometry comes from each `cameras[]` entry's `fov_deg` and `mount_pitch_deg` (70° and level when unset).
- `GET /api/vision/status` carries `depth`: the model state (`ready`, `no_model` or `unavailable` with a reason), its run interval, and per camera whether the grid and the zones run, their leases, the last scale source and the run count.
- `GET /api/vision/models`: each installed file a registry variant names carries that model's `task` and `registry_id`. The model manager picks a recommended model per task (`recommended_model(task)`); the installer's detector pick uses it.
- Dashboard: Settings > Vision edits the depth layer (on or off, rate, threads).
- TUI: the Vehicle tab's Autonomy block shows a Depth row (where the grid and zones run and their scale, `idle`, or `no model`).

### Changed

- `PUT /api/vision/detector` refuses a model the cached registry gives a task other than detection (a depth model) with 422.

## [0.108.15] - 2026-11-07

### Added

- Cockpit: the Feed is laid out in the cockpit grid from the cockpit spec (`layout.json`), with the spec's breakpoints (compact < 600, medium 600–839, expanded 840–1599, tv ≥ 1600 or the kiosk). Across the top is the top bar: flight-state chip, status pill, battery with time to reserve and the RTL-reserve marker, link, navigation source, controller, layer level, settings and the Emergency stop. The left column holds Take off / Land, Return home and Pause; the right column holds the shutter, photo/video switch, zoom, gimbal recentre and camera settings. The bottom-left card switches between map, attitude with radar, and the obstacle radar (from the pilot's 72 clearance sectors). The telemetry row (D, H, H.S, V.S, AGL) is at the bottom and the skill picker at the bottom right. Triangle or `V` cycles the views (Pilot, Map, Split, Vision). Portrait stacks the video and top bar over the controls. A television or the kiosk scales the chrome by 1.33 and always shows the focus ring.
- Cockpit: the flight-state chip evaluates the spec's `flight-state.json` rules on the pilot state: READY / NOT READY with a "Set up aircraft" link, PILOT · profile, the skill and its detail, GOTO, MISSION · WP n/N, MISSION PAUSED with Resume, RTH, LANDING, and FC · mode with Take control. A second line shows the hold reason, AVOIDING / BLOCKED and the control mode. The chip dims to LINK when the pilot state is older than 2 s, and tapping it opens the Mode sheet. The Mode sheet holds the profile, the control mode (hold to switch; ELRS only on a ground-station link), avoidance (Off needs a hold), hand to the FC in Loiter, PosHold or AltHold, take control, the VTOL transition, and every FC mode. Any change there stops a running motion skill.
- Cockpit: the Launch sheet sets the take-off altitude (TKOFF_ALT on a plane), what to do after take-off (hold, a behaviour skill, start the loaded mission, or go to a point) and an optional land / return when done. Holding Launch sends the pilot's `launch` action, and a step strip shows checks, arm, take off and after. Abort brakes to a hold.
- Cockpit: a pre-flight checklist gates the first arm, take-off or launch of a session. It lists the pilot's readiness items, the link, GNSS, battery, home, the controller (when ADOS Pilot flies the sticks) and on-board storage.
- Cockpit: flow overlays. A 3 s handover banner shows the pilot's banner text from the spec strings. "Centre sticks to take control" has a progress ring. "Sticks are with <surface>" comes with a hold to take the sticks over. The link-loss countdown and the cancellable low-battery RTH countdown are full-width banners. Optional voice prompts (on by default) speak handovers, RTH, low battery and mission-state changes.
- Cockpit: the skill picker replaces the skill bar. It has Behaviors, Camera, Navigation and Utility tabs, with each plugin's skills grouped under its name. A skill opens its Tweak panel, with its parameters as steppers, sliders and segmented choices that honour `visible_if`, then GO and Stop. Camera one-shots fire directly, and the D-pad, Cross and Circle drive the picker. follow-me gets a FOLLOW | ORBIT control and the pilot's avoidance chip, both in the picker and on the follow rail.
- Cockpit: DualShock 4 and other standard-layout controllers, mapped from the spec's `ds4.json`. A single poller handles the radial deadzone, per-pad calibration, Mode 1/2, tap / hold / double-tap gestures and combos (arm, kill, PS brake and RTH hold, Options + Circle), plus rumble. A pad the browser does not report in the standard layout is calibrated first. On a drone in ADOS Pilot mode the cockpit streams the sticks to `WS /api/v1/pilot/stream` at 50 Hz. It stops and sends `release` when the pad disconnects, the tab is hidden, or the pad goes 150 ms without reporting. On a ground-station kiosk whose native input owns the sticks (`native_sticks`), the browser streams nothing and ignores the pause, RTH, arm and kill bindings. Optional on-screen sticks work in landscape.
- Cockpit: Settings › Controls. It draws the controller from the spec art with leader-line labels and live input: pressed controls, stick dots in their deadzone rings, and trigger bars. Pick a control on the drawing, or with "Press a control", and set an action per gesture. Two controls on one action, or one control and gesture bound twice, outline red and block Save. The safety bindings (pause, the RTH hold, the arm and kill combos) can only be moved. It also holds stick calibration with a deadzone slider, a rumble test, reset, export and import, the on-screen sticks and voice-prompt switches, and a first-connect tour. On a ground-station kiosk the bindings are also written to `PUT /api/v1/ground-station/input/bindings`.
- Dashboard: Settings › Aircraft setup renders the agent's flight-controller setup wizard (`/api/v1/fc-setup` and its stream). Each step shows its title, body, illustration, live values, progress, actions (hold-to-confirm and "Propellers are removed" tiers), blockers and results. The page also offers per-step re-runs, abort, revert to the parameter snapshot and the setup report. A drone whose FC has no completed ground setup shows a banner linking to it. The cockpit renders the same wizard under Settings, from the chip's "Set up aircraft" link, and on the kiosk.
- TUI: the Vehicle tab shows the pilot's owner, the control mode with the profile, and the avoidance state with the nearest clearance.

### Changed

- Cockpit: the built-in skills are the spec's `builtins.json` (arm, disarm, take off, land, return home, pause, resume, take control, emergency stop), each with its arm requirement, confirm tier and stale-link rule. With the pilot running they go to it as actions; without it they go to `POST /api/command`. The mode presets left the bar for the Mode sheet. Extension skills are validated against the strict skill schema: a skill missing a required field, using an unknown category or declaring a non-config activation is rejected and listed. `default_binding` keys are honoured, and a key collision leaves the later skill unbound and listed. Only one motion skill runs at a time. Pause, land, RTH, emergency stop and Mode-sheet changes stop motion skills only. A write shows pending until the plugin echoes it and warns after 3 s. State older than 3 s dims and shows "last seen". A running skill stays stoppable while the link is down. A plugin that stops is removed with a notice.
- Dashboard: the dashboard is the setup surface and the cockpit is the flight surface, so the dashboard gets the aircraft setup wizard and no stick or flight controls.

## [0.108.14] - 2026-11-07

### Added

- Cockpit: the Feed draws the cockpit data layers over the video in one canvas between the target boxes and the instrument HUD: the agent's projected route (next waypoint gate with distance and ETA, the following waypoint, legs, home, off-screen arrows), the motion items (flight path vector, planned follow path coloured by the avoidance state, follow goal, orbit ring), per-camera obstacle nearness cells with a bottom-centre obstacle arc, and, when switched on, model depth cells and landing-zone candidates. The route follows the agent-computed mission state: paused greys and dashes it, a loaded plan previews while disarmed or under pilot or FC control, returning and complete hide it. Frames never re-render the page: a change-driven frame loop paints only when something changed, interpolates keyed items between frames, dims a layer after 0.5 s without data and replaces it with an "unavailable" chip after 2 s, and caps the kiosk display at 30 frames a second. `O` cycles the layer level (Full, Route, Off).
- Cockpit: the Feed holds a view lease on the nav stream (`{"type":"view","camera_id","layers"}`, renewed every 2 s) for the on-screen camera and its enabled layers, and drops it when the page is hidden, so the agent computes these layers only for what is watched. Depth and landing zones each hold a vision demand lease (`POST /api/vision/demand` with `kind`) while on.
- Cockpit: the Vision view's side panel: live detections, depth and surfaces switches, engine health and the detector model picker.
- Cockpit: the nav stream parses the `pilot` kind (command authority state) for the flight-state chip and sheets.
- Cockpit: demo mode (`?demo=1`) feeds every data layer and a pilot state.
- Dashboard: the video panel draws the route, motion and obstacle layers at the Route level, read only, from a 5 Hz `GET /api/v1/nav/status?view=main:…` poll while live video shows. The dashboard gets no layer controls: it is the setup surface, and the cockpit is the flight surface.

## [0.108.13] - 2026-11-06

### Changed

- The onboard detector runs on a camera only while a consumer needs it there. A consumer holds a lease per camera that expires on its own: a plugin through `ctx.vision.request_detection(camera_id, hold_s)` (Rust SDK `request_detection(camera_id, hold)`, wire method `vision.request_detection`, capability `vision.track.designate`), an operator surface through `POST /api/vision/demand` `{camera_id, hold_s, owner?}`. Repeating a request renews the lease, `hold_s: 0` releases it, and the longest hold is 60 s. An operator designation holds detection on its camera for `vision.designate_hold_s` (default 30 s). Frame capture, the frame bus, video and every other frame consumer keep running on every camera. `vision.detection_demand: always` restores detection on every captured camera.
- `vision.detector.rate_hz` defaults to 3 Hz per camera when absent; an explicit `0` keeps the unpaced behaviour (every frame the accelerator is free for).
- The ONNX backend runs each session on a fixed thread budget with spin-waiting off: the detector on `vision.detector.threads` intra-op threads (default 1), the re-id model on one, inter-op on one. The RTSP frame decoder runs on `vision.decode_threads` ffmpeg threads (default 1). The runtimes' defaults (a thread per core, spin-waiting between nodes) contended with the flight stack for the CPU on a companion board.
- Detections whose box covers more than `vision.max_box_area_frac` of the frame (default 0.5) are dropped before tracking and publishing, for every backend. A person that close is under a metre from the lens and never a follow target, and the small person detector reports full-frame boxes on close ground texture.
- `GET /api/vision/status` carries a `detection` block: the demand mode and, per camera, whether detection runs and the leases holding it on (`owner`, `remaining_ms`).
- `GET /api/vision/detections/latest` takes an optional `camera_id` and returns that camera's newest batch; without it the reply is unchanged.
- Cockpit: the Feed's target overlay (main view and PiP) holds a detection lease on the camera it shows while on screen (`POST /api/vision/demand`, renewed every 2 s, released on close or camera change), and says "Detector idle" or "Detector unavailable" when the engine is not detecting on that camera.
- Dashboard: Settings > Vision edits when detection runs (`vision.detection_demand`), the designation hold, the largest-box cutoff, decoder threads, and, with a detector selected, its rate and inference threads.
- TUI: the Vehicle tab's Autonomy block shows the detector row ("idle" or "detecting: main"), read from `/api/vision/status`.

### Fixed

- Dashboard: numeric config fields write a JSON number, so an optional key that is still unset takes a value.

## [0.108.12] - 2026-11-05

### Security

- A plugin sending `SET_GPS_GLOBAL_ORIGIN` (MAVLink message 48) now needs `estimator.pose.inject` on top of `mavlink.write`. The message moves the flight controller's EKF origin, which shifts every position the estimator reports, so it is gated like the other estimator inputs. A send holding only `mavlink.write` is refused with `capability_denied: estimator.pose.inject`.

## [0.108.11] - 2026-11-05

### Fixed

- A capture plugin can start a capture session as soon as the agent is back up after a restart. The supervisor now writes the capture-storage file (`/run/ados/capture-storage/capture-storage.json`) before it reports ready, and plugins start only after that. Before, the first write came several seconds after the plugins had started. A `capture_start` in that window found the file missing (after a boot) or stale (after a quick restart) and was refused with HTTP 400 `E_STORAGE_FULL`.
- `GET /api/status` now reports the node's `profile` (wire form, such as `drone` or `ground-station`) and its ground-station mesh `role` (null on other profiles). Both come from the same source as `GET /api/pairing/info`. Before, the route had no `profile` field, so a client reading it got `profile: null` on a drone.

## [0.108.10] - 2026-11-05

### Fixed

- `POST /api/v1/ground-station/wfb/pair` checks that the decoded key blob is 64 bytes before the fleet gate and refuses any other length with 400 `E_INVALID_KEY_BLOB`. A wrong-length key used to answer 409 `E_FLEET_KEY_MISMATCH` on a paired station, and on an unpaired one reserved a fleet slot and then rolled it back.
- The station-wide unpair (`DELETE /api/v1/ground-station/wfb/pair`) now also removes the fleet roster (`/var/lib/ados/fleet.json`) and the shared beacon key (`/etc/drone.key`), as its documentation already said it did. The next fleet no longer inherits the old members' slots, and the presence plane returns to cold start instead of authenticating as the old fleet. A roster or key that cannot be removed is a 500 `E_UNPAIR_FAILED`.
- The pair route's documentation states the full guard order, that `drone_device_id` is required, and that the route is LAN-only (relay-forbidden).

## [0.108.9] - 2026-11-05

### Fixed

- `GET /api/video/config` serves the `hopping` block from the hop-supervisor snapshot only while that file is inside its 10 s freshness window; past it, the route returns the config stub. A dead ground listener's last `key_status` and hop history no longer read as live. The drone radio rewrites its `key_unavailable` hop snapshot every 5 s, instead of once, so it stays fresh.

### Changed

- The link key row draws `bound` in the neutral value colour on the dashboard, cockpit and TUI, rather than as healthy: it means this station's own key reads, not that both ends hold the same key. The cockpit Link screen polls `/api/video/config` only on a ground station.

## [0.108.8] - 2026-11-05

### Added

- The ground station's link key state (`hopping.key_status` in `GET /api/video/config`: `bound`, `cold_start` or `key_unavailable`) is shown on the dashboard's WFB Receive card, the cockpit's Link screen and the TUI Link tab's WFB radio card. Cold start and an unreadable key are drawn as warnings; the row is hidden when the field is absent (drone side, or before the first beacon is heard).

## [0.108.7] - 2026-11-05

### Fixed

- The dashboard's video card reads `ready` instead of `no_camera` on a node whose cameras are all declared in the `cameras:` roster without a local V4L2 device (a `capture: true` entry the vision engine opens, or an `rtsp://` network source the video pipeline pulls) while the `main` stream is not yet publishing.
- A vision camera whose frame source opens but delivers no frames is now visible. The engine logs a warning, `frame source delivered nothing; reopening`, with the camera id and how long it was silent, and records the error `delivered no frame for 10 s` for that camera until the next frame arrives. `/api/vision/status` gains two additive fields from the engine's status file: `source_errors`, a list of `{camera, error}` for every camera whose source is failing or silent (empty when the file is absent), and `live_inputs`, the number of cameras currently delivering frames (null when the file is absent).

### Changed

- The generated CLI reference normalises each command's help text with `inspect.cleandoc`, so `scripts/emit-reference.py --check` gives the same result on Python 3.12 and 3.14.

## [0.108.6] - 2026-11-05

### Fixed

- `ados-vision.service` is capped at 512 MiB instead of 256 MiB. Its cgroup holds the vision engine and the `ffmpeg` processes it starts for each `rtsp://` capture camera (a demuxer and a decoder, about 60 MiB for a 1280x720 stream). The ONNX detector and re-id arenas also grow on the first inference. Two such cameras with the detector and re-id went past 256 MiB within seconds of the first frames, so on a host without swap the kernel OOM-killed the engine every few seconds and the cameras reconnected over and over. Measured peak with that load: 324 MiB.

## [0.108.5] - 2026-11-05

### Fixed

- A capture camera with an `rtsp://` source now delivers frames when its stream carries the H.264 parameter sets (SPS and PPS) only in the SDP (`sprop-parameter-sets`), as an encoder writing global headers publishes them behind a server that forwards its RTP packets as sent. `ados-vision` copies the stream to Annex-B with one `ffmpeg` and decodes it with a second; the copy dropped the parameter sets the demuxer held as extradata, so the decoder saw only slices naming a PPS it never got and decoded nothing. The source then went silent for 10 s, was reopened, and the cycle repeated, so the camera never reached the frame bus and `ados-video` never got a publisher for it. The copy now writes the parameter sets in-band before every keyframe (`dump_extra=freq=keyframe`, as the agent's own encoders do).

## [0.108.4] - 2026-11-05

### Fixed

- A plugin MCP tool invoke (`POST /api/plugins/{plugin_id}/tools/{tool}/invoke`) no longer fails with `tool_handler_missing` when the plugin declares a service. Each declared service connects to the plugin host under the plugin's id beside the main process, and the host routed every invoke to whichever session connected last, usually a service with no tool handler. The host now keeps every live session of a plugin, asks them newest first, and passes over a session that answers `tool_handler_missing` (nothing ran there), so the call reaches the process that serves the tools. Any other answer is final, so a tool still runs at most once.

## [0.108.3] - 2026-11-05

### Fixed

- Installer health check no longer reports `mediamtx-missing` on workstation and compute installs, which never receive mediamtx; the check now follows the same per-profile fetch catalog, so those installs stop showing `degraded` for it.

## [0.108.2] - 2026-11-04

### Fixed

- **A profile switch no longer leaves the old profile's units running.** A
  ground station reinstalled as a drone could keep `ados-mediamtx-gs` running,
  holding the ports the drone's media server binds, so drone video stayed at
  `connecting`; the reverse switch could keep `ados-video` running the same
  way. The installer stopped those units while the previous profile's
  supervisor was still up, the supervisor restarted them when they died, and
  the supervisor restart that followed carried them over. The installer now
  disables the other profile's units, then stops the supervisor, stops those
  units and starts the supervisor again.
- **An install with `--profile` switches the profile the agent runs.** The
  installer rewrote `profile.conf`, but every service reads `agent.profile`
  in `config.yaml` first, so a ground station reinstalled as a drone went on
  running as a ground station. An explicit `--profile` now rewrites that key
  too (comments and layout kept); an absent key or `auto` still defers to
  `profile.conf`, and an install without `--profile` leaves it alone.
  `ados update` pins the profile the services run (`agent.profile`, then
  `profile.conf`, or the running agent's when the config is not readable)
  rather than `profile.conf` alone, so on a node whose profile was changed
  through the setup API before this release the update keeps that profile
  instead of rewriting the config back to the one `profile.conf` still names.
- **A profile change through `POST /api/v1/setup/apply` switches the units
  too.** With `auto_restart` it only restarted the supervisor, so the old
  profile's units stayed enabled and any still running were carried over, and
  the new profile's units were never enabled. It now runs
  `ados-supervisor switch-profile` as a transient unit, which does what an
  install of the new profile does to the units: stop the supervisor, stop and
  disable the other profile's units, enable the new profile's, start the
  supervisor and kick the new profile's start set. It also points
  `profile.conf` at the new profile, so a later bare `--upgrade` keeps it.
  The per-profile unit lists now live in one place, `ados_protocol::units`,
  shared by the installer and the switch. A ground-station role change on the
  same profile still restarts the supervisor. The injector-arbitration key
  follows the profile the way an install applies it: a switch to a drone or
  visor removes a ground station's `true` (until now the config writer refused
  that switch outright) and a switch into a ground station turns it on.
  A profile change applied without `auto_restart` no longer advises a plain
  restart, which would leave the old profile's services enabled: the reply
  says to apply again with restart on or to run
  `sudo /opt/ados/bin/ados-supervisor switch-profile` on the node. The batch
  apply also stops answering "Restart the agent to apply. Restarting agent."
  for one request.
- **The dashboard and cockpit apply a profile change at once, and say what it
  stops.** Both always send `auto_restart: true`; the "Restart services after
  apply" switch is gone, because leaving it off left the node half-switched.
  The confirm says the node moves its services to the new profile, that
  leaving the ground-station profile turns off its HDMI kiosk, and that a
  switch to a drone stops the node's Wi-Fi access point and drops anyone on
  it. On the kiosk screen itself the cockpit adds that the screen turns off
  and does not come back. The TUI's profile page uses the same wording.
- **Config writes from the residual API no longer report failure after
  landing.** The handover file for the config side effects was created in
  `/tmp`, which the API's sandbox keeps read-only, so every changing write from
  it (a setup profile, network or UI change among them) was saved but answered
  "not saved" and skipped its side effects. The file now lives in the run
  directory, and a handover that cannot be created is logged instead of
  failing a write that already landed.

## [0.108.1] - 2026-11-04

### Fixed

- **Ground-station pairing installs the shared radio key again.** The control
  service runs read-only outside its listed paths, and `/etc` was not among
  them, so a ground-station pair answered `"shared_key": "write_failed"` and
  the drones' presence beacons never linked; a ground-station factory reset
  could not delete `/etc/drone.key` or `/etc/gs.key` either. `/etc` is now
  writable for `ados-control`, as it already was for `ados-supervisor`.

## [0.108.0] - 2026-11-03

### Added

- **Navigation and map APIs owned by the agent.** Every operator surface reads
  autonomy state from the agent itself instead of an extension's iframe.
  - `WS /api/v1/nav/stream` sends `{kind, ts_ms, data}` frames for `pose`,
    `scan`, `nav`, `follow` and `map`, each kind at most 10 Hz with the newest
    value winning, plus `follow_event` frames for `follow.reacquired` and
    `follow.lost`. A new client first gets the latest frame of every kind. The
    upgrade takes the API key or a one-shot ticket of the new `nav.stream`
    scope.
  - `GET /api/v1/nav/status` returns `{ts_ms, age_ms, data}` per kind, or
    `null` for a kind nothing has published.
  - `GET /api/v1/map/live?since=<seq>` returns the live occupancy tiles that
    changed after `seq` (an empty list when none did).
  - An MCP token reads both `GET` routes with the `read` scope.
- **Plugin event socket.** The plugin host exports bus events on a root-only
  unix socket (`/run/ados/plugin-host/_events.sock`, contract `plugin.events`
  v1): a client subscribes to topics and receives one newline-JSON line per
  event, with the publisher id stamped by the host. The navigation APIs read
  from it; with no plugin host running, every kind stays `null`.
- **Per-profile plugin resources.** A manifest may set
  `agent.resources_by_profile: {<profile>: <resources>}`. A plugin's unit on a
  node of a listed profile runs under that envelope, anywhere else under
  `agent.resources`. Unknown profiles, unknown keys, a profile listed under two
  spellings, and an envelope above its own class ceiling are refused.
- Plugin units get `ADOS_PLUGIN_MEMORY_MAX_BYTES`, the same limit as their
  `MemoryMax=`, so a plugin can size its caches without reading its cgroup.
- **Vision backend state.** `GET /api/vision/status` carries `backend_state`
  (`ready`, `mock` only when configured, or `unavailable`) and
  `backend_reason`.
- **Person detector for the ONNX CPU backend.** The model registry lists
  `person_yolox_tiny_v1`: YOLOX-Tiny (Apache-2.0), person-only, one 640×640
  ONNX variant (`runtime: onnx`, `head: yolo5`) for boards without an NPU.
  `scripts/models/export_yolox_person.py` reproduces it from the upstream
  checkpoint: the graph decodes its own boxes, carries the `ados.*` metadata
  the ONNX backend reads at load (head layout, classes, input size), and the
  script prints the size and sha256 the registry records. The
  `detect_bench` example times a detector over a folder of recorded frames.
- **Re-identification embeddings for the tracker.** Track appearance comes
  from a re-id model (`vision.reid`) when one is loaded, otherwise from a
  16-bin colour descriptor of the box's upper and lower halves. Both are
  L2-normalised and compared by cosine similarity, and each detection names
  its source in `embedding_model` (the model id, or `histogram`). Off unless
  `vision.reid_enabled` is set. The OSNet re-id models in the registry
  (`scripts/models/export_osnet_reid.py` reproduces the ONNX one) are marked
  `benchmark_only` because their weights carry research-use terms, so the
  colour descriptor is the shipped default.
- **Runtime-aware model selection.** The model manager picks only variants
  this board can run: `rknn`, `hailo` or `onnx` as configured, or under
  `auto` the NPU first when the board declares NPU TOPS, then ONNX. NPU TOPS
  break ties within a runtime. A registry model marked `benchmark_only` is
  never auto-provisioned or recommended.
- **Flight-controller telemetry for onboard autonomy.**
  - On the `drone` profile, outside a capture session, the router requests
    the streams onboard autonomy steers from: local NED position and velocity
    (20 Hz), the downward rangefinder, faster attitude, and GNSS.
  - Telemetry gains an `ekf` block (`EKF_STATUS_REPORT` flags) and a
    `local_position` block (x, y, z, vx, vy, vz). Each carries `age_ms` and is
    `null` until the message arrives.
  - `fc_msp_armed` reports an MSP flight controller's armed flag, read
    observe-only from the status replies a poller draws through the MSP
    proxy. It is `null` when nothing was read in the last 5 s, never "disarmed".
- The `ekfSourceSet` command switches the EKF3 source set (`args: [set]`,
  1–3) with `MAV_CMD_SET_EKF_SOURCE_SET`.
- **Operator API key rotate and clear.** `POST /api/v1/security/api-key/rotate`
  replaces `security.api.api_key` and returns the new key once with
  `rotated_at`. `DELETE /api/v1/security/api-key` clears it; the pairing key
  keeps working. The old key stops working at once. Both need operator
  authentication, are refused through a relay, are not reachable by MCP
  tokens, and log `security.api_key_rotated` / `security.api_key_cleared`
  without the value.
- **Autonomy block in the terminal UI.** The Vehicle tab shows the follow
  phase, lock, range ± sigma and re-acquire mode; the navigation source, the
  external-nav health and odometry rate; the lidar's nearest obstacle and scan
  rate; and the map session. It is polled from `/api/v1/nav/status`, and every
  state is a symbol plus a word.
- **The cockpit gains the dashboard's features.**
  - Console building blocks: scaffold with hint bar, two-pane, sheet, dialog,
    menu, one toast host, guarded button, slide-to-confirm, setting rows.
    Gamepad, button and touch focus work the same in all of them.
  - Layouts follow four width classes (compact, medium, expanded, TV or
    `?kiosk=1`). Settings add glove mode (larger touch targets) and a dark,
    light or night-vision theme.
  - New screens: Logs, Peripherals, Parameters, MSP, Video, Transmit, IO,
    GNSS, LoRa, Capture, Sensors and Sources. Workstation and compute nodes
    get a Node home. Every destructive action needs a guarded press or a slide
    to confirm.
- **HUD on the cockpit Feed.** Each target state has its own box style and
  text label: candidate, tracked, following, coasting, searching, re-acquired
  and lost. Tap a box to act on it, or long-press and drag to designate one.
  The Feed also shows a follow range panel, a navigation-source chip (with a
  manual EKF source-set switch), the strict/smart re-acquire toggle, lidar
  scan dots on the proximity radar, a local-frame minimap with live occupancy
  and the SLAM trail, and a belly-camera picture-in-picture with its own
  overlay.
- Camera role `down`, labelled "Belly" in ground-station views: a downward
  camera for mapping texture, downward optical flow and precision landing.
- **The terminal UI is a full operator console.** `ados` (and `ados-tui`) now
  opens a tabbed console instead of a read-only cockpit: Overview, Vehicle
  (Ground on a ground station), Link, Video, Params, Extensions, Logs, System
  and Settings, each shown only on the profiles that have it.
  - Settings mirrors the web dashboard's pages (identity, network and Wi-Fi,
    radio, video, cloud and remote access, pairing, security and MCP tokens,
    region, battery, self-heal, extension parameters) plus a schema-driven
    browser of every config key. ←/→ applies a toggle or short choice at once;
    Enter edits; every write goes through the agent's HTTP API.
  - Params edits flight-controller parameters with the ArduPilot and PX4
    metadata catalogs: search, groups, staging, `.param` import and export,
    and a sequential write that is refused unless a fresh reading says the
    vehicle is disarmed.
  - `ados-tui --host <host[:port]>` drives an agent on another machine. A
    paired agent asks for its API key (optionally remembered per target in
    the user's `tui.json`); an unpaired one asks for the dashboard PIN. On-box
    quick actions are hidden remotely.
  - Themes from the brand tokens (amber, brand dark, brand light, NVG),
    colour-blind mode, Unicode/Nerd Font/ASCII glyph sets, motion that reduces
    itself over SSH, a command palette (Ctrl-K), toasts with a notification
    history, and copy to the local clipboard over OSC 52.
  - No flight commands are sent from the terminal. Lifecycle actions (service
    restart, reboot, update, extension changes, Wi-Fi joins, profile and region
    changes, unpair) refuse while the vehicle is armed or its arm state is
    unknown.
- The REST client in `ados-protocol` carries the error body of a non-2xx
  reply, sends a dashboard PIN session when one is set, and opens
  server-sent-event streams. `ParamsResponse` now matches the `/api/params`
  wire shape.
- The terminal UI's MCP page offers a Docker command (`y` copies it) that runs
  the published `ghcr.io/altnautica/ados-mcp` image, alongside the
  from-source command. The Docker command targets the agent's LAN address,
  since `.local` names may not resolve inside a container.
- Mesh pairing (`ados-mesh-pairing`), mesh bring-up (`ados-batman`), the
  setup captive portal (`ados-setup-captive`) and the HDMI kiosk
  (`ados-kiosk`) run as Rust: `ados-groundlink --pairing`,
  `ados-groundlink --mesh-bringup`, and the new `ados-captive` and
  `ados-kiosk` binaries. Unit names, the pairing socket protocol, the
  pairing snapshot and the pair-event journal are unchanged; the Python
  services are removed.
- `ados-term` (MIT OR Apache-2.0): the terminal toolkit the installer runs
  on (themed frames, `/dev/tty` and crossterm backends, live step progress,
  wizard widgets, command runner).
- JSON Schemas for `/api/status/full`, the cloud heartbeat and
  `/api/v1/setup/status` under `schemas/api/`, and TypeScript types
  generated from them for the dashboard, the cockpit and Mission Control.

### Changed

- More writes are refused with `409 E_ARMED` while the vehicle is armed
  (`"force": true` overrides):
  - `PUT /api/config` of the flight-controller link keys `mavlink.source`,
    `serial_port`, `baud_rate`, `system_id` and `component_id`;
  - `POST /api/video/config` carrying a link-tuning knob (`fec_k`, `fec_n`,
    `mcs`, `preset`, `auto`, `tier_idx`).
- A plugin's published-state sidecar file is written at most twice a second
  per plugin and topic. A trailing write keeps the newest value.
- The dashboard and the cockpit share one copy of the agent API clients, the
  locale and the toast bus (`web-shared/`).
- The quick actions overlay no longer carries Pair, Logs or Unpair (the
  Pairing settings page and the Logs tab replace them), and actions no longer
  have single-key shortcuts on the main screen.
- Process-group handling for every child the agent spawns (wfb, video
  pipelines, dnsmasq, socat, batctl, systemctl probes) is one shared
  implementation: null stdin, kill on drop, SIGTERM then SIGKILL to the whole
  group, capped stderr logs, and line readers that bound line length and
  survive non-UTF-8 output.
- `/api/status/full` is built from a typed response; the wire keys are
  unchanged.
- The `ground-station` Python extra drops packages nothing imports.

### Fixed

- **A build without a detector backend ran the mock and reported it as
  working.** A backend that cannot run now resolves to `unavailable`, never
  to the mock, and logs `vision_backend_unavailable` at error level.
- The sensor bus's copy of the capture contract lagged its upstream: `rig.json`
  validation now knows the `lidar` scan model and requires it on a `lidar`
  sensor.
- `docs/api-surface.md` marks every route under a relay-forbidden path as
  relay-forbidden, as the agent enforces it, not only exact matches.
- The log store keeps the most recent quarantined copy even when two
  corruptions land inside one clock tick or the clock was stepped back:
  quarantine suffixes now strictly increase and the prune orders by them,
  not by modification time.
- **The web dashboard showed every node as unrestricted.** The region page,
  the Home region chip and the LoRa region hints read the regulatory posture
  from the setup status, which never carries it, so a pinned region looked
  unrestricted and could not be removed. They now read
  `network.regulatory` from the config.
- **Applying a self-hosted cloud setting blanked the MQTT broker and reset
  the port.** The cloud page seeded the form from the cloud choice, which has
  neither; it now seeds from `server.self_hosted` in the config.
- **ArduPlane vehicles got the copter parameter catalog in the dashboard.**
  The agent reports fixed-wing and VTOL airframes as `Fixed Wing` and
  `VTOL …`, which never matched "plane"; those labels now select the plane
  catalog.
- The terminal UI's Ground control link group names the endpoints by kind
  (a desktop ground station on TCP 5760, a browser GCS on WebSocket 8765).
- The terminal UI's help overlay lists "Search everything" once instead of
  once per terminal encoding of Ctrl-/, and the MCP token row fits 120
  columns.
- Dependency updates: rustls 0.23.46, rustls-webpki 0.103.16,
  webpki-roots and webpki-root-certs 1.0.10.
- **A cloud plugin install verified the archive against the manifest
  hash.** It now verifies against `archiveSha256` and refuses a job without
  one. An empty or malformed pin is refused everywhere instead of skipping
  the check.
- The aux RPC response listener retries a failed bind every 3 s instead of
  stopping for good, binds with the command-socket mode, and pauses after an
  accept error.
- A frame-bus reader could read a slot's length before the acquire fence.
- Disconnected IPC clients are dropped when they hang up, not on the next
  broadcast, so a socket that never broadcasts no longer leaks a handle and
  a descriptor per connection.
- Promoting a new agent source tree moves the old one aside and puts it back
  if the swap fails, instead of deleting it first.
- Revoking a plugin grant takes effect on the plugin's next request.
- Radio process respawn waits 2 s; the MQTT transport stops its event loop
  when dropped; `batctl` neighbour queries name the mesh interface.
- `factory-reset.sh` refuses to run without root and stops every agent unit
  before wiping; `install.sh` bounds `wget` and refetches once from scratch
  after a failed verification.

- **The simulator clock never locked.** Simulator time from the video SEI is
  paired with the frame's arrival on the host, so every pair carries the
  encode, transport and decode latency: one-sided, jittery, often over
  100 ms. The least-squares fit read that jitter as the clock jumping and
  restarted every few seconds, so no session got the `sim` timebase.
  - `ados-clock` fits the SEI pairs to the lower envelope of `host - source`
    (the convex-hull line that minimises the summed lateness of the 60 s
    window), keeping only the least-late pair per 50 ms so the window spans
    the same time whatever the camera count and frame rate. It locks once the
    pairs span 40 s with an envelope residual under 5 ms.
  - The envelope sits one pipeline floor behind the true relation, which
    one-way pairs cannot measure. A measured floor goes in the new
    `clock.sim.latency_floor_ns` and is added to the offset; unset, the
    published sigma (the source row, `/api/v1/clock`, the bus clock sample and
    so the session manifest) carries a 30 ms bound for it.
  - A pair up to 1 s behind the envelope is taken without counting toward a
    restart. Three pairs more than 1 s behind, or ahead of it beyond
    `max(5 sigma, 1 ms)`, within 10 s restart the fit: a simulator restart or
    a lower latency floor relocks instead of biasing the line.
  - A simulated sensor hub runs on simulator time. On a node with
    `simulation.enabled`, `ados-sensorhub` marks a simulator-board hub's sync
    pairs `sim_time` (new optional field on the `sync` bus payload; the board
    id alone is not trusted, as it is also what erased flash reads as), and
    the clock takes a locked simulator hub as the exact `sim` source ahead
    of the SEI envelope: the hub service maps its samples through the same
    pairs and frames keep their own simulator stamps, so the floor cancels.
    `clock.json` gains `source` (the row the timebase follows) and a per-row
    `sim_time`.
  - When the timebase's source changes (the simulator hub drops and the
    envelope takes over, say), the clock logs `clock_source_changed` (warn
    when a source was lost or replaced) and `clock.json` carries the newest
    change as `source_changed`, with the session-time step when both sources
    share the timebase, so a running capture can record it.
  - The flight controller, flight hub and GNSS PPS fits are unchanged.

## [0.104.0] - 2026-10-20

### Added

- **Front panel on a visor; GNSS, radios and LoRa on the panel and TUI.**
  The panel config routes (`/api/v1/ground-station/ui*`) accept a visor and
  serve profile defaults from one shared table: the visor screens are the
  capture HUD plugin page, cameras, sensors, storage and about, and its B1/B2
  short presses carry the new `passthrough` action, which only plugin button
  subscribers act on. `ados-pic` (the button reader) runs on a visor. The
  LCD tab bar follows the configured screens; new LCD pages `cameras`,
  `sensors`, `storage`, `details.gnss`, `details.radios`, `details.lora` and a
  `shutdown_prompt` power-off confirmation backed by
  `POST /api/v1/setup/shutdown` (refused over the cloud relay: a remote
  power-off strands the node). The I2C OLED status frame gains
  `gnss <fix> <sats>` and `radios <n>` and rotates through the configured
  `gnss`, `radios` and `lora` screens when those services are present; the
  LoRa views show region, duty use and the transmit state with its reason.
  The TUI adds a per-profile row (visor capture, storage, clock and hub IMU;
  ground-station radios, GNSS and LoRa; drone sensor rates).

- **ROCK 5B+ board profile, ROCK 5C sensor bindings.** `rock-5b-plus`
  (detected from `radxa,rock-5b-plus` / "Radxa ROCK 5B+"; a plain ROCK 5B
  does not match) declares both 4-lane CSI connectors with their Radxa
  Camera 4K overlays, GNSS on UART7-M1 (`/dev/ttyS7`) with PPS on header pin
  16 (GPIO 100), the USB sensor hub, and panel buttons on header pins
  29/31/33/35. The shared ROCK 5C / 5C Lite profile gains its CSI connector,
  GNSS on UART8-M0 (`/dev/ttyS8`) with PPS on header pin 16 (GPIO 37), and
  panel buttons on header pins 29/31/33/35. Neither declares a battery gauge:
  the shunt is the rig's, bound under `battery.gauge` in the agent config.
  The ROCK 5B+ builds the RTL8812EU driver on device.

- **Panel buttons follow the board's GPIO numbering.** A board profile's
  `buttons` are the board's GPIO numbers: BCM lines on a Raspberry Pi,
  Linux GPIO numbers on Rockchip, read from `/dev/gpiochip<n / 32>` line
  `n % 32`. B1..B4 follow the up/down/select/back order, so the panel button
  tables apply on every board, and the button stream's `label` comes from
  that order. A board profile's `reserved_gpio` lists lines bound to a bus,
  console or panel; a button on one of them, or on the GNSS PPS line, fails
  the profile. A board that is not a Raspberry Pi and declares no buttons has
  no button reader (`buttons_not_declared`) instead of falling back to the
  Raspberry Pi lines. Plugin button presses carry the panel `label` (`B1`..)
  next to the board `pin`.

- **PPS overlay at install.** On a ground station or visor whose board
  profile declares `gnss.pps_gpio` for a serial receiver (never on a drone
  or other profile, and never for a sensor-hub receiver, whose pulse the hub
  captures itself), the installer enables a pps-gpio overlay: the stock
  `pps-gpio` overlay on a Raspberry Pi, a generated and `dtc`-compiled
  `ados-pps` overlay on Rockchip boards (Radxa `/boot/dtbo` + `u-boot-update`,
  Armbian `user_overlays`), and loads the `pps-gpio` module. Uninstall
  reverses each of these edits and removes the chrony drop-in. Not yet
  validated on hardware.

- **Dashboard pages for GNSS, LoRa, capture and sensors.** Ground station:
  `/gnss` (receivers, survey/off base mode, RTCM out); ground station and
  drone: `/lora` (region, TX line with pause countdown, one meter per
  governor rule, refusals, admin region picker with confirm, locked while
  armed, beacons, module band / ELRS domain / Meshtastic / airborne chips).
  Visor:
  `/capture` (REC/STOP/MARK through the world-engine plugin route, health
  chips, refusal codes) and `/settings/capture` (storage, exposure). Drone
  and visor: `/sensors` and `/settings/sensors` (hub, rig upload, clock
  sources). A node with an unrecognised profile sees a neutral
  "not available on this node" state. The cockpit ignores `passthrough`
  button actions, which are reserved for plugins.

- **Plugin capabilities `sensor.stream.read` and `capture.storage.write`.**
  Both are enforced by the generated plugin unit. `sensor.stream.read` binds
  the `/run/ados/sensing` directory (bus socket `sensors.sock`, sidecar
  `clock.json`) read-only into the plugin sandbox and joins the new
  `ados-sensors` group, never `ados-operator`. `capture.storage.write` binds
  the `/run/ados/capture-storage` directory (janitor sidecar
  `capture-storage.json`) read-only,
  `/run/ados/capture` read-write, the host capture root (resolved at render
  time from the janitor sidecar, the persisted `auto` choice, else
  `/var/ados/capture`) at `/run/ados/capture-root`, sets
  `ADOS_CAPTURE_ROOT_HOST` / `ADOS_CAPTURE_ROOT_MOUNT`, and joins the new
  `ados-capture` group, which now owns `/run/ados/capture` (`2775`). Both
  bind directories, never single files: a file bind stays on the inode
  present at unit start, so a sidecar replaced by rename or a socket
  re-created by a restarted `ados-clock` would go stale inside the plugin.
  The installer's run-dir tmpfiles rule creates both directories at boot.
- **Plugin capabilities `video.whep.relay` and `sensors.rig.write`.** Gated
  at dispatch. `video.whep.offer {camera_id, sdp}` resolves a configured,
  enabled camera id to the media leg publishing it (`main` for the primary
  camera), posts the viewer's SDP offer to the media server's loopback WHEP
  endpoint for that leg, and returns the answer plus an opaque
  `/whep/<session>?camera=<leg>` location; any other id is refused as
  `unknown_camera`;
  `video.whep.close` tears down only a location the host returned to the same
  plugin. `sensors.rig.write` stores a rig through the control plane's
  `PUT /api/v1/sensors/rig` over its Unix socket and returns the route's
  status and JSON body. SDK: `ctx.video.whep_offer` / `whep_close` and
  `ctx.sensors.put_rig`.
- **Plugin capability `camera.control.write`.** Gated at dispatch.
  `camera.control.set` sets exposure, gain or auto exposure on a camera
  configured `capture: true`: the host relays it over `vision.sock`, and the
  engine applies it on the capture thread's own device handle (the device is
  never opened a second time), reads every touched control back and answers
  `{applied, exposure_us, gain, reason}`. `applied` is true only when each
  requested control reads back as asked (exposure and gain within one driver
  step); the returned values are the read-back ones. `gain` is a multiplier,
  mapped linearly onto the driver's codes with its default code as 1×; a gain
  control without a usable default answers `gain_unit_unknown`. The engine
  waits `ENGINE_WAIT_MS` (1.5 s) for the capture thread and drops a request it
  stopped waiting for, so a late request is never applied after its answer.
  Cameras not configured for capture (the video tap among them) answer
  `not_capture_camera`, RTSP rig cameras `not_v4l2`, a driver without the
  control `control_unsupported`. SDK: `ctx.camera.set_controls`.
- **SDK: `ctx.tools.serve(handler)`.** The Rust plugin SDK answers host
  `tool.invoke` requests (MCP tools declared by a plugin) through a
  registered handler; a plugin without one answers `tool_handler_missing`.
- **Sensor bus and host clock.** New seam `/run/ados/sensing/sensors.sock` (contract
  `sensor.v1`): length-prefixed msgpack frames `{v, kind, src, origin,
  payload}` whose clock, IMU, GNSS and trigger payloads are the capture
  contract `ados.capture.v1` protobuf messages, every `t_ns` on the host
  monotonic clock. `ados-clock` (drone, ground station, visor) brokers the
  bus, fits each time source (flight controller, sensor hub, GNSS PPS,
  simulator) against the host clock over a 60 s window, publishes 1 Hz
  `ClockSample`s, writes `/run/ados/sensing/clock.json` and the per-stream rate table
  `/run/ados/sensors.json`, and feeds a locked PPS to chrony through a SOCK
  refclock (installer step `chrony_ados` writes `/etc/chrony/conf.d/ados.conf`).
- **REST: `/api/v1/clock`, `/api/v1/sensors`, `PUT /api/v1/sensors/rig`
  (stored at `/etc/ados/rig.json` after validation against the capture
  contract rig schema), `/api/v1/storage`, `/api/v1/gnss`,
  `PUT /api/v1/gnss/base`, `POST /api/v1/gnss/rtcm`.** A route whose service
  is absent answers `present: false`. `/api/status/full` gains the booleans
  `clock`, `sensor_hub`, `gnss` and `capture_storage`.
- **Flight-controller sensor lane.** The router requests HIGHRES_IMU 200 Hz
  (RAW_IMU 50 Hz fallback), ATTITUDE_QUATERNION, LOCAL_POSITION_NED,
  GPS_RAW_INT, SYSTEM_TIME and SCALED_PRESSURE on a visor, and on a drone
  while `/run/ados/capture/active` exists; runs a min-RTT TIMESYNC estimator;
  publishes IMU (FLU), barometer and CAMERA_FEEDBACK triggers on the sensor
  bus; and injects every `rtcm` bus frame to the flight controller as
  fragmented GPS_RTCM_DATA. When capture ends, every stream only the capture
  table asked for returns to the flight controller's default rate.
- **GNSS service.** `ados-gnss` (ground station, visor) reads receivers over
  serial, UDP (simulator bridge) or the sensor hub; decodes UBX NAV-PVT/DOP/
  SVIN/RELPOSNED, TIM-TP, MON-VER and NMEA GGA/RMC; pairs PPS pulses with UTC
  for the clock; runs an RTK base (survey-in or fixed position via
  CFG-VALSET) whose RTCM3 output goes onto the sensor bus toward paired
  drones; and reports moving-baseline heading. State in `/run/ados/gnss.json`,
  commands on `/run/ados/gnss-cmd.sock`. Config block `gnss`.
- **Capture storage.** Config `storage.capture_root` (`auto` picks the first
  NVMe volume with 64 GiB free), `capture_budget_pct`, `capture_floor_free_gib`
  and `capture_evict_unuploaded`. The janitor's `capture` category evicts
  uploaded sessions oldest first, never the recording one, measures the write
  rate, and writes `/run/ados/capture-storage/capture-storage.json`, whose `state: full`
  refuses new sessions with `E_STORAGE_FULL`.
- **Sensor hub service.** `ados-sensorhub` reads the sensor hub's protocol v1
  stream over USB CDC (`1209:ad05`) or TCP (a simulated hub), sends the hub its
  `Config`, maps hub time onto the host monotonic clock (per-second lower
  envelope of arrival pairs, published as `sync` frames), and publishes IMU
  (body FLU, SI units, rotated with the stored rig extrinsics), trigger,
  barometer, magnetometer and GNSS passthrough frames on the sensor bus.
  `rtcm` frames on the bus are forwarded to the hub. Link state is written to
  `/run/ados/sensorhub.json`. Config block `sensors.hub` (`transport`:
  `auto` | `serial:<device>` | `tcp:<host>:<port>`, `imu_odr_hz`,
  `trigger_hz`, `exposure_us`, `gnss_forward`); an explicit transport sets
  the `/etc/ados/sensorhub-enabled` marker.
- **Sensor hub and battery gauge in `board.json`.** The board probe reports a
  `sensor_hub` object (`present`, `device`, `vid_pid`) and touches
  `/run/ados/sensor-hub-present` while a hub is attached, and copies a
  profile's `battery_gauge` binding. Board profiles accept optional `imu`,
  `gnss`, `sensor_hub` and `battery_gauge` bindings. USB discovery classifies
  `1209:ad05` as `sensor_hub`.
- **Field tile packs on the ground station.** `GET
  /api/v1/ground-station/tiles` lists the PMTiles archives in
  `/var/ados/tiles` (`ADOS_TILES_DIR`) as `{name, bounds, minzoom, maxzoom,
  tile_type, usable, size_bytes, etag}`, and `GET
  /api/v1/ground-station/tiles/{name}/{z}/{x}/{y}.mvt` serves one vector tile
  as stored (gzip `Content-Encoding`, one-day `Cache-Control`, `ETag`; `204`
  for a tile the pack does not hold). Ground-station profile only; the tile
  route needs the API key but is not charged to the per-caller request
  budget. New capability flag `tiles.pack`. Installer flag `--tile-pack
  <url|path>` places a pack in that directory.

- **LoRa beacon lane (`ados-lora`).** Opt-in (`lora.enabled`, marker
  `/etc/ados/lora-enabled`) on drone and ground-station nodes. Transports
  `e22` (Ebyte UART module), `meshtastic` (serial client API) and `loopback`
  (UDP, for simulation). Every frame passes a per-region governor built from a
  compiled band-plan table (32 rows; uncited regions are receive-only): region
  from `lora.region`, else `network.regulatory.region`, else transmit blocked
  (`region_unset`); module-band check; `max_power` (ERP/EIRP/conducted, PSD,
  antenna gain, adaptive power control); `dwell_time`; per-sub-band
  `duty_cycle` over a rolling window with a persisted, fsync'd
  charge-then-send airtime ledger (`/var/ados/lora/airtime.jsonl`) so a
  restart cannot reset the budget; `lbt`. E22 frames are charged at twice the
  computed time on air. REST `GET/PUT /api/v1/lora` (PUT is admin scope; a
  region change is refused while armed), `GET /api/v1/lora/regions`,
  `GET /api/v1/lora/beacons`; sidecars `lora.json`, `lora-beacons.json`;
  `lora` flag in `/api/status/full`.
- **Multiple WFB adapters.** `radio.adapters[]` assigns roles
  (`rxtx|rx|tx|mesh`, matched by MAC or USB path; empty = auto). Every rx/rxtx
  adapter feeds `wfb_rx` (receive diversity); `wfb-stats` v2 carries
  per-adapter `adapters[]` and `diversity`; `GET /api/v1/radio/adapters`.
- **Several ELRS modules.** `radio.crsf.modules[]` (`pilot|autonomy`);
  `crsf-stats` v2 and `GET /api/v1/ground-station/crsf` carry `modules[]`
  with per-module link statistics and the reported ELRS `domain`.
- **INA226 battery gauge.** A `battery.gauge` binding in the agent config
  (`type`, `i2c_bus`, `addr`, `shunt_ohm`, `max_current_a`, all required), else
  a `battery_gauge` binding in `board.json`, adds a pack with
  `"source": "ina226"` to `GET /api/v1/battery`. A binding missing its shunt
  values leaves the gauge absent with a logged reason.
- **Rotary encoder on the front panel.** A board profile may declare a KY-040
  class encoder as `encoder: {a, b, sw}` (CLK, DT and push switch, in the same
  GPIO numbering as `buttons`); `/run/ados/board.json` publishes it as
  `encoder`, and a line that is reserved, repeated or shared with a button
  fails the profile. `ados-pic` reads it next to the buttons: a full-detent
  quadrature decoder (four transitions per detent, bounce cancels out, a
  transition with both lines changed is rejected) reports each detent as a
  `short` press labelled `ENC_CW` or `ENC_CCW`, and the push switch is
  debounced and classified short/long like a button under `ENC_SW`. Default
  actions: `ENC_CW_short` `cycle_screen`, `ENC_CCW_short` the new
  `prev_screen`, `ENC_SW_short` `confirm`, `ENC_SW_long` `back`, remappable in
  `ground_station.ui.buttons.mapping`; the panel config routes list them
  when the board declares an encoder. Plugin button subscribers receive the
  encoder events with those labels. The ROCK 5C / 5C Lite profile declares an
  encoder on header pins 36/38/15 (GPIO 130/133/140) and the ROCK 5B+ on
  header pins 36/38/40 (GPIO 105/106/107). An encoder line that cannot be
  requested or read (busy, missing) only drops the encoder; the buttons keep
  working. The cockpit maps `enc_cw`/`enc_ccw` to next/previous and the
  switch to activate (long press: back). The panel input reader now waits on
  every line at once, so a press on one button is no longer held until
  another button sees an edge.
- **Node lane flags in the cloud heartbeat.** The heartbeat carries `gnss`,
  `lora` and `sim` from the same predicates as `/api/status/full`, so a
  relay-only node shows the same tabs as on the LAN.

### Changed

- **Framebus descriptor v2.** `ts_ms` is replaced by `ts_ns` (host
  monotonic) plus optional `ts_sensor_ns`, `exposure_ns`, `trigger_seq` and a
  `timebase`; formats `gray8`, `gray16`, `bayerrggb8`, `bayerrggb16` are
  added; `read_slot` is replaced by the buffer-reusing `read_slot_into`. The
  vision tap header (v2) carries `ts_mono_ns`. Detection batches now stamp
  `ts_ms` from the frame's monotonic time.
- **`mavlink.sock` v2.** Every frame the router fans out carries an 8-byte
  little-endian `rx_mono_ns` header (host monotonic read time) inside the
  length prefix; client writes are unchanged.
- **Sensor bus readers.** `/run/ados/sensing/sensors.sock` is now `0660
  root:ados-sensors`. The broker classifies each peer: services (root, its
  own uid) publish anything, `ados-operator` members sensor data but not
  `sync`/`rtcm`, and members of `ados-sensors` only subscribe. The installer
  creates `ados-sensors`, adds the operator to it, and removes it on purge.
- **Capture root ownership.** The supervisor's janitor and the installer keep
  the capture root at `3775 root:ados-capture` (an existing root's owner and
  mode are corrected; the sessions under it are not touched), so a sandboxed
  capture plugin can create session directories; the sticky bit stops one
  writer renaming or removing another's session.
- **SDK `tool.invoke` always answers.** A handler that panics answers
  `tool_handler_panicked`; a result that cannot be framed answers
  `tool_reply_unencodable: ...`.
- **Plugin file modes.** An archive member or fetched payload installs `0755`
  when the manifest runs it or its content starts with an ELF or `#!`
  header (a helper the plugin launches itself, such as a payload under
  `bin/<arch>-linux/`); every other file installs `0644`.

### Fixed

- **Field tile budget exemption needs a valid key.** Only a tile read that
  presents a valid `X-ADOS-Key` skips the per-caller request budget; a
  keyless or wrong-key tile read is charged like any request. The tile pack
  list also releases the memory map of a pack deleted from the directory.
- **`--tile-pack` checks the archive.** The installer stages the pack beside
  its destination and refuses a file that is not a PMTiles v3 archive,
  leaving any existing pack in place.
- **A captured camera's device stays the vision engine's.** The video
  service's primary encoder takes the primary camera's configured device when
  discovery found it and never a device a `capture: true` camera owns, and
  the vision engine no longer reads the video tap as a second camera when the
  primary camera is itself captured.
- **Front-panel writes on a visor.** The installer fetches `ados-pic` on a
  visor (its unit was skipped for want of the binary), and the control plane
  serves the PIC, gamepad and Bluetooth writes natively on every profile that
  runs the panel daemons.
- **Flight-controller streams after capture.** A stream only the capture
  table asked for is reset to the flight controller's default rate when
  capture ends, not disabled.
- **Sensor rate verdicts.** The hub sidecar publishes the configured trigger
  rate as `trigger_hz_configured`, and `GET /api/v1/sensors` takes a hub
  camera's `expected_hz` from it (null when unknown), so an under-delivering
  trigger is flagged. A ground station's `fc` block is filled (the profile
  check took the hyphen spelling), and an idle rig GNSS sensor reports
  `source: gnss` where `ados-gnss` runs.
- **Visor OLED.** A visor's I2C panel draws its own home (capture state,
  clock, free space), cameras, sensors and storage screens instead of the
  ground-station link frame. The LCD cameras page shows each camera's source,
  and the storage page shows only the volume marked as capture storage.
- **LoRa transmit state.** `tx_allowed` is false whenever `blocked_reason`
  is set, transient blocks included, and clears as soon as the rule frees.
  `PUT /api/v1/lora` answers once the lane reports the new settings (`504`
  when a running lane has not within 8 s), never the pre-change body.
- **Dashboard.** The GNSS page is available on a visor; the capture exposure
  fields render empty, not `null`, before capture runs.
- **WHEP relay takes a camera id.** `video.whep.offer` resolves the camera
  id to its media leg and refuses any id that is not a configured, enabled
  camera.
- **Uninstall** removes the chrony drop-in and reverses the PPS overlay.

### Security

- **Capture session removal never follows a link.** The janitor removes an
  evicted session through directory descriptors opened without following
  symlinks, re-checks right before removal that the session is still a real
  directory, and unlinks a link it meets instead of descending into it, so a
  capture writer that swaps its session for a symlink cannot have the root
  janitor empty the link's target.
- **System and agent directories are refused as the capture root.** An
  explicit `storage.capture_root` that is relative, carries a `..`
  component, is `/` or a shared parent (`/var`, `/tmp`, `/home`, `/mnt`,
  `/media`, `/srv`, `/opt`), or lies under `/etc`, `/usr`, `/bin`, `/sbin`,
  `/lib*`, `/boot`, `/dev`, `/proc`, `/sys`, `/run`, `/root`, `/opt/ados`,
  `/var/lib`, `/var/log`, `/var/cache`, `/var/spool`, or `/var/ados` outside
  `/var/ados/capture`, is logged and resolved as `auto`. The same check gates
  the janitor's owner and mode fix, the installer's drop-in and the plugin
  sandbox's read-write bind of the root.
- **A visor never commands its flight controller as a vehicle.** On the
  `visor` profile the control service answers every flight action (the
  command route, CAN passthrough, RC channel injection, taking
  pilot-in-command, each also through the relay proxy, and a plugin tool
  declared `flight`) with `409` `E_NOT_A_VEHICLE`. The MAVLink router drops
  client frames that change mode, upload or steer a mission, carry manual or
  RC override input, set attitude, position or actuator targets, carry raw
  CAN or CAN FD frames, or carry a movement, arming, motor-test, actuator,
  CAN-forward, servo or relay command, counting them in
  `visor_flight_frames_dropped`; the swarm layer holds without sending or
  counting. Disarming (plain or forced, the command route's `disarm` and
  `killswitch`) and flight termination stay allowed, so a hub that is somehow
  armed can always be stopped. A frame an on-box socket writer splits across
  two writes is reassembled per connection before the check, as the TCP and
  WebSocket proxies already do; an MSP board gets client bytes untouched.
  Parameter reads and writes, calibration, message and stream requests, time
  sync, signing and log download are unchanged, and drones and ground
  stations behave as before.

## [0.103.3] - 2026-10-14

### Added

- **Swarm bus loopback transport.** `ADOS_SWARMBUS_TRANSPORT=loopback` runs
  `ados-swarmbus` without a monitor-mode adapter: the exact frames the
  `AF_PACKET` backend injects travel over UDP to a shared-medium relay
  (register on control TCP `ADOS_SWARMBUS_RELAY_PORT`, default 7900; frames
  on the next port; deliveries on a local downlink socket). The kernel BPF
  program is evaluated in userspace on receive, so another fleet's frames and
  non-swarm frames are dropped where the kernel would drop them. Settings:
  `ADOS_SWARMBUS_RELAY_HOST`, `ADOS_SWARMBUS_RELAY_PORT`, `ADOS_SWARMBUS_NODE`
  (required, unique per node), `ADOS_SWARMBUS_NODE_ADDR`,
  `ADOS_SWARMBUS_LISTEN`. An unknown transport value is refused. Unset keeps
  the native radio.

## [0.103.2] - 2026-10-13

### Added

- **Simulation mode flag.** `simulation.enabled` (default `false`) marks a node
  wired to a simulated flight controller and camera. `/api/status` and
  `/api/status/full` report it as `sim`.

## [0.103.1] - 2026-10-11

### Changed

- **Bundled extension catalog** is pinned to the published release archives
  (Vision Navigation 0.5.0, Follow-Me 0.2.10, SIYI Pod 0.5.1, MAVLink Gimbal
  1.4.1, World Engine 1.0.1) with their sha256 digests.
  `scripts/sync-plugin-catalog.py` refreshes it and `--check` fails when it
  lags; a weekly workflow runs the check. Entries with no published release
  are dropped.

## [0.103.0] - 2026-10-11

One day and 56 commits after 0.102.6. The range is a review pass over the
whole agent: safety interlocks, the native HTTP edge, the radio and network
services, plugin trust, video, and the installer. Most changes fix a surface
that showed one thing while the node did another, and the agent now reports
what it measured instead of what it was configured to do.

Three threads run through it. First, the vehicle's armed state now guards
every route that can drop the link or restart a service: a setup apply, a
config key that restarts the radio, a plugin install or a network change
answers `409` while armed unless the caller asks to force it, and the
installer refuses an armed upgrade without `--force-armed`. Second, the
agent addresses the flight controller by the system id it actually heard,
holds an optional MAVLink signing key of its own, and tells the operator when
the autopilot will reject its unsigned traffic. Third, the agent got lighter:
three Python services are gone, ground video and `/whep` are served natively,
and edge installs fetch every binary from one signed manifest.

Large source files were split into smaller modules across the daemons, the
Python tree and the dashboard, with no change in behaviour, and a size check
keeps them that way.

### Added

- **Armed interlock.** Setup apply, link-affecting config keys, the units that
  carry the radio or uplink, plugin lifecycle routes and network routes answer
  `409 E_ARMED` while the vehicle is armed. Send `"force": true` in the body or
  `?force=1` to proceed; the dashboard offers a confirm-and-force retry.
- **Hero leases on `/api/video/profile`.** `POST
  {"profile":"hero","holder":"<id>","lease_s":N}` holds a drone on the hero
  profile for 5-120 s (default 30) and renews on repeat;
  `{"profile":"thumbnail","holder":"<id>"}` or
  `DELETE /api/video/profile?holder=<id>` releases it, and an expired lease
  reverts the encoder with no further request. A plain set (no holder) still
  moves the base profile. Every reply carries `base_profile` and `leases`.
  While a vision engine reads the frame tap the drone holds an internal
  `vision` lease, so vision never sees a thumbnail-rate stream.
- **Flight controller identity.** Commands, parameters, camera control and
  signing address the system id the router latched from the autopilot, and
  `MAV_SYSID` is accepted. An optional fleet id reconciler (off by default)
  reports its outcome in a status sidecar. Land picks the right command for
  the vehicle type.
- **Agent-held MAVLink signing.** Enrolling the flight controller with
  `agent_holds_key` stores the key on the node (mode 0600) so the agent signs
  its own frames, with persisted timestamps. Off by default.
- **New status blocks.** Full status carries the flight controller identity,
  the proxy listening posture per edge, the unsigned-command risk, the
  injector arbitration mode and the agent signing state.
- **Plugin failure reasons and locales.** Plugin list and detail carry
  `failure_reason`, cleared by a successful enable. Plugin locale bundles are
  served at `/api/plugins/{id}/locales/{lang}`.
- **`ados plugin logs --service`** picks one plugin unit, and the command
  prints a hint when the journal is not readable by the caller.
- **Profile redetect.** A route reruns profile detection and the detection
  result is kept as a snapshot.
- **Model downloads as jobs.** A model download returns `202` with a
  `status_url` to poll.
- **Recording segments.** Recordings are written as segments, and the ground
  station recording result lists `segments` and `interrupted_count`.
- **Video passthrough and mock target.** A camera that already emits H.264 can
  be passed through without re-encoding, and a mock target is available for
  vision testing.
- **Telemetry subscribe in the Python SDK**, and a per-stream telemetry
  capability for extensions.
- **Reference exporters.** The CLI, routes, config keys, capabilities,
  manifest fields, boards, systemd units, installer flags and dashboard routes
  are exported as reference tables for the documentation, with a drift check.
- **Display render examples.** The OLED frame builder lives in the display
  library, and render examples cover the receiver mesh page and the role
  picker with synthetic data.

### Changed

- **Native `/whep`.** WebRTC playback is served by the native control service;
  the Python WHEP proxy is gone.
- **Native ground video.** `ados-groundlink --ground-video` serves ground
  station video with a native UDP/RTP source and a stall watchdog.
- **Edge installs.** CI publishes one cached release build as `rev-<sha>` and
  a signed `prebuilt-main/manifest.json`. An edge install resolves every binary
  from that manifest and refuses bytes that do not match its sha256 and size.
- **One unit table** drives the supervisor registry, the installer unit lists,
  the restart allowlist and the armed interlock.
- **Config writes** are validated against the full model in the native writer,
  with enums and cross-field rules; the schema covers every native key. A
  drone refuses `mavlink.injector_arbitration`, which needs a ground station.
- **Services skipped on an unmet start condition** (for example the Hailo unit
  on a board without the accelerator) read `idle` with reason
  `condition_unmet` instead of inactive.
- **Mesh capability** is derived live from the attached radios.
- **Uplinks.** Cellular and wired uplinks are resolved by driver, modem bearer
  liveness and static IPv4 are handled, failover is immediate, and a route
  apply is checked and re-applied until it lands (`route_applied` in uplink
  state).
- **Radio.** TX power is bounded by the ceiling and reports the measured
  value; the hop channel survives a respawn; drone and ground share one
  regulatory posture; revoked relays are excluded and the mesh key is rotated.
- **Router.** A per-client framer forwards unknown messages, replies addressed
  to one client go to that client, the listener retries its bind, the serial
  port to the flight controller is exclusive, and WebSocket messages are size
  capped. The banner request repeats until the autopilot is identified.
- **Video.** The software encoder fallback is no longer permanent, a failing
  encoder restarts alone, SEI is injected natively, a lost source recovers in
  about 2 s, and mediamtx keeps arrival timestamps so readers get the first
  frames at once.
- **Status** samples CPU in the background and caches the slow blocks.
- **Front panel.** The touch reader keeps retrying, OLED status reads support
  contrast and re-init, and button pins come from the board file.
- **Plugin units and log names** are read from the host, driver kinds map to
  catalog capabilities, and the endpoint type defaults to websocket.
- **Dashboard.** Vision backends come from the schema, pairing and flight
  controller tiles were added, polling pauses when the tab is hidden, and every
  visible string goes through translation keys.
- **Installer.** The config header links the configuration page, template keys
  are trimmed, retired repository keys are pruned, the migration step waits for
  its lock, the closing summary prints each failed step's reason, and a
  rollback restores the previous wheel and config. A non ground station install
  removes the ground station avahi service file. The terminal UI update follows
  the install channel.
- **`ados update`** checks the armed state first; WFB apply goes through the
  native routes.

### Fixed

- **Cloud fields** that the node could not read are reported as absent instead
  of with placeholder values.
- **Detections** report `available: false` when no vision engine runs.
- **A LAN pair** is honoured at once, and the dashboard pairing tile reads
  paired for a LAN-paired node in local cloud mode.
- **Cloud relays** restart when pairing changes, and the pilot-in-command
  state is refreshed every watchdog tick.
- **Profile set** keeps the other config keys; the setup access point retries
  until it comes up.
- **MAVLink 2 messages** with extension fields are accepted by the decoder.
- **Cockpit flight timer** starts only on an observed arming edge, and the Pair
  screen reports invite read failures.
- **Log store** keeps disk headroom.
- **Supervisor restart** no longer stops the services systemd is already
  restarting with it, so each service starts once and a service left off is not
  reported as failed.
- **CRSF lane** stops when it is turned off instead of reloading and staying
  resident.
- **Parameter cache** is cleared when a different flight controller connects,
  and a complete download drops names the controller no longer reports, so a
  cache left by a previous controller no longer inflates the count or marks a
  partial download complete.
- **Terminal dashboard** counts services from the full status, so the Services
  panel and the overall health reflect the running units.
- **Dashboard security settings** read the pairing state: a paired node shows
  its key as set, and a configured second key is labelled as such.
- **Dashboard logs** say when the log store is off and how to turn it on,
  instead of an empty list with a buffer hint.
- **Peripheral matching** can check a sysfs attribute of the matched device
  node (`match.sysfs`). The SPI LCD seed matches the panel's framebuffer driver
  and the UVC camera seed matches the `uvcvideo` driver, so a console
  framebuffer or a codec node no longer reads as a connected device.
- **Idle exits are final.** `ados-video` on a ground station (or with video
  off) and `ados-setup-captive` once setup is done exit cleanly and stay down,
  instead of being restarted every 3 s.
- **One mDNS record per node.** A ground station no longer installs a second
  static `_ados._tcp` record; the control service's advert covers every
  interface, the access point included, and an upgrade removes the old file.
- **HDMI cockpit on a ground station** reads the flight controller link from
  the node status, so flight skills are available when the link is up.

### Security

- **Config reload fails closed.** Security settings reload live, and an
  unreadable config refuses off-box mutations with `E_CONFIG_UNREADABLE`.
- **Node-wide PIN budget.** Failed dashboard PIN attempts count against the
  whole node, keyed by IPv6 and tunnel peers too; the lockout reads
  `E_PIN_LOCKED` with `scope: "node"` and `retry_after_s`.
- **Upload digests.** Large uploads are signed over `X-Content-SHA256` and the
  streamed bytes must match it; one archive is admitted at a time (`429`
  otherwise) and metadata is capped.
- **WebSocket tickets** are available to an unpaired node's PIN session; the
  same-origin setup wizard shortcut works only while unpaired.
- **Plugin trust.** Board identity matches by HAL profile, SoC or wildcard;
  first-party keys are pinned; revocation works by signer, archive hash or
  release and is merged daily; revocation and auto-update wait until disarmed;
  rollback and crash restore are guarded; plugin units cannot open the flight
  controller serial port; installs are checked for tampering on reconcile and
  enable; the plugin socket checks peer credentials; manifests are checked
  strictly.
- **Swarm setpoints** are gated on pilot-in-command, and swarm rekey clears
  stale rows.
- **Phone receiver invites** refuse low-order keys.
- **The `ados-api` sandbox** is narrower, the log store authenticates before
  rate limiting, support bundles are redacted, and the pairing code is no
  longer written to the log.

### Removed

- **`ados-discovery`, `ados-health` and `ados-wifi-client`** units and the
  Python code only they reached, plus unused dependencies.
- **The Python ground media package** and the Python WHEP proxy.
- **The captive portal token route.**
- **Dead MQTT config keys** and retired OLED keys (now rejected).

### Notes

For Mission Control and other API clients:

- **Guarded routes** may answer `409` with
  `{"error":"E_ARMED","message":"Vehicle is armed","override":"force"}`.
  Retry with `"force": true` in the body or `?force=1` after the operator
  confirms.
- **`E_CONFIG_UNREADABLE`** is returned for off-box mutations while
  `config.yaml` cannot be read.
- **`E_PIN_LOCKED`** now carries `scope: "node"`: the lockout applies to every
  client of that node.
- **Uploads** should send `X-Content-SHA256` (lowercase hex). A mismatch
  answers `401`; a busy upload slot answers `429`.
- **New wire fields:** `fc_system_id` (status and ground station slots),
  `mavlink_proxy_posture` with `tcp_listening`, `udp_listening` and
  `ws_listening`, `agent_commands_unsigned_rejected_risk`,
  `injector_arbitration`, `agent_signing`, `agent_holds_key` on signing
  enrol, `route_applied` in uplink state, `segments` and `interrupted_count`
  on recordings, `failure_reason` on plugins, `status_url` on a `202` model
  download, `available` on detections, `reason: "condition_unmet"` on service
  rows, and `base_profile` and `leases` on the video profile.
- **Removed units:** `ados-discovery`, `ados-health` and `ados-wifi-client`
  no longer exist; an upgrade drops them.
- **`/whep`** is served natively at the same path.
- **Edge installs** read `prebuilt-main/manifest.json` and fetch assets from
  the `rev-<sha>` release it names.
- **Upgrading while armed** needs `--force-armed` on the installer.

## [0.102.6] - 2026-10-10

### Added

- Ground station phone receivers: a phone can ask to receive the WFB link
  directly. The operator approves each request (shown with the phone's key
  fingerprint) in Mission Control, the dashboard pairing page or the HDMI
  cockpit Pair screen; the receive bundle is sealed to the phone's key and can
  be fetched once. The routes are ground-station only and refused over the
  relay.
- HDMI cockpit: Kill, Pause and Resume skills; confirm tiers (hold, slide,
  guarded Kill) usable by touch, panel buttons and gamepad; takeoff sends the
  chosen altitude; extension skills, video overlays, panels and widgets with a
  sandboxed host that checks granted capabilities; alerts by level. The Feed
  gains a safety band (reach, arm, mode, battery with time to reserve, GPS,
  link, video, preflight, recording and flight timers, guarded Kill) and a
  canvas HUD (5° pitch ladder, roll arc, flight-path marker, heading tape,
  speed and altitude tapes with REL/MSL, vertical speed).
- Extension manifests may omit the GCS entrypoint when they contribute only
  skills or parameters; map overlays and mission templates are accepted.
- GCS capabilities `ui.slot.cockpit-widget` and `vision.track.designate`.

### Changed

- The cockpit, dashboard, terminal UI, installer and display pages use the
  generated brand palette; the cockpit and dashboard share one web code base
  (video transport, session, API client).
- The kiosk opens the cockpit (`/cockpit/?kiosk=1`) by default.
- Dashboard: "Plugins" is now "Extensions" (`/extensions`), routes load on
  demand, unknown paths show a not-found page, and the wake lock is held only
  while video is visible.
- Cockpit Feed polling drops to about 6.5 requests a second on a drone with no
  extensions; detection and button sockets back off instead of reconnecting in
  a loop.

### Fixed

- Dashboard client routes load on an unpaired node instead of returning JSON.
- WHEP sessions are deleted when the video stops.
- HLS gives up and falls back after repeated fatal network errors.
- Command skills are enabled only while the local flight controller link is
  live; a ground station relaying a drone says so instead of offering commands
  that would be refused.
- A battery reported as unknown no longer raises a low-battery warning.

## [0.102.5] - 2026-10-07

### Fixed

- A stalled consumer of the MAVLink, MSP or state socket is now evicted after
  a bounded backlog. Each accepted client's kernel send buffer is pinned at
  256 KiB instead of following the system default, which the installer raises
  to 4 MiB for video, so a reader that stopped draining held up to megabytes of
  stale frames before the slow-client eviction fired.

## [0.102.4] - 2026-10-05

### Added

- **x86_64 Linux installs from local builds.** The installer now accepts an
  x86_64 Linux host. No release binaries are published for x86_64, so the
  install needs `--channel edge --artifacts <dir>` pointing at a workspace
  built on the host; without it the binary step fails with that instruction,
  and a service missing from the directory is never fetched from the release.
  `install.sh` still refuses x86_64 and names the build-from-source route.
- A `generic-x86_64` board profile, used by both HAL halves for an x86_64
  host with no board identity (CPU-only video, USB UVC navigation camera).

### Fixed

- A downloaded release binary is now checked against the host architecture
  before it is placed, as a local artifact already was, and the rebuild hint
  names the host's own target triple.

## [0.102.3] - 2026-10-03

### Changed

- Dependency updates: axum 0.8 with tower-http 0.6, getrandom 0.4, base64
  0.23, toml 1, sd-notify 0.5, hmac 0.13, blake2 0.11, chacha20poly1305 0.11
  and sha2 0.11, plus the minor-version group (including ort 2.0.0-rc.13).
  The WFB key fingerprint is unchanged: it is still BLAKE2b with an 8-byte
  digest, checked against fixed reference values.
- The dashboard and cockpit build with Vite 8 and @vitejs/plugin-react 6; the
  dashboard uses lucide-react 1. Both bundles are rebuilt.
- CI actions move to their current majors.

## [0.102.2] - 2026-10-03

### Changed

- The bundled extension catalog pins ADOS World Engine 1.0.1.

## [0.102.1] - 2026-10-03

### Fixed

- **Legacy uplink NAT.** Every install and upgrade now removes the NAT older
  builds left behind: `-A POSTROUTING -o <iface> -j MASQUERADE` rules in the
  live table and in `/etc/iptables/rules.v4`, and a `table ip ados_nat` block
  in `/etc/nftables.conf`, which gains an `include "/etc/nftables.d/*.nft"`
  line if it has none. The firewall manager's own `ADOS_NAT` chain and live
  `ados_nat` table are left alone. The step is optional: a failure is reported
  and the install continues.
- **MAC-pin overrides keyed by interface name.** An override under
  `network.mac_pin.overrides` keyed by an interface name (`wlan1`) was ignored
  at runtime. The upgrade moves it to the adapter key
  (`<vidpid>@<usb_path>`) recorded for that interface in
  `/etc/ados/mac-pins.state`; a name the state file cannot resolve stays as it
  is. The installer's MAC-pin step now runs after config migration so the
  reconcile sees the translated keys.

### Internal

- Rust tests keep their scratch files in a `TempDir` instead of leaving them
  in the system temp directory.
- The Python test fixture isolates the config writer's lock path, so the
  suite passes on a Linux host without `/run/ados`.
- The prebuilt driver pipeline builds the Debian 11 kernel rows from a pinned
  Debian snapshot, and the Raspberry Pi rows verify the archive with the
  vendor's SHA-512 keyring.

## [0.102.0] - 2026-10-03

### Security

- Relay tickets are bound to the request's method, path and body hash, carry a
  single-use nonce, and tolerate up to 300 s of clock skew. Every refusal
  answers 401 with `{"error":"E_RELAY_TICKET","reason":…}`. The relay denylist
  now holds only trust-root and credential changes; pairing claims refuse any
  relayed request.
- WebSocket tickets are single-use. Pairing writes are serialised across the
  Rust and Python writers with a shared file lock.
- The configured API key is accepted the same way on native routes,
  WebSockets and the dashboard PIN. `/api/v1/setup/status` needs a credential
  once the node is paired.
- Each plugin runs as its own system user in group `ados-plugins`, receives
  its capability token through a systemd credential, and cannot reach any
  node-local address except its own declared ports. Plugin units cannot
  execute outside their install directory and start only after the network
  guard is loaded.
- A plugin update must come from the signer of the installed version. A
  revoked signer disables its plugins at startup and when the revocation list
  changes. The download allowlist no longer admits S3 or `localhost`.
- Release assets without a signature are refused on every channel; only a
  local `--artifacts` install may skip the signature. CI refuses to publish
  unsigned binaries.
- The residual Python API listens only on its internal Unix socket. Mesh
  networks join with SAE (802.11s) or IBSS-RSN and refuse an open join.

### Changed

- **Device id.** A node's device id is now the full id from
  `/etc/ados/device-id` everywhere: the pairing info, the cloud and MQTT
  identity, mDNS, relay targets and presence beacons. A short `agent.device_id`
  left in `config.yaml` is ignored when the file exists. A cloud-paired node
  must pair again after upgrading, and ground-station fleet slots registered
  before the upgrade must be released and the drones paired again: the fleet
  registry now matches device ids exactly. One node used to answer to two ids,
  and the drone refused every relayed request (hero selection, relayed status,
  fleet slot delivery) addressed by the id its own pairing info reported.
- **Armed interlock.** Reboot, shutdown, factory reset, profile change,
  ground-station unpair, radio channel and TX power writes, parameter writes,
  CRSF parameter writes, and restarts of the MAVLink, radio, WFB, groundlink
  and supervisor services answer 409 `E_ARMED` while the vehicle is armed or
  its state is older than 3 s. Send `"force": true` (or `?force=1`) to
  override.
- **RC loss.** When no stick source is live, the CRSF lane stops sending RC
  frames, so the receiver and flight controller run their own failsafe.
  Gamepad liveness is checked by polling the device state.
- **Fleet MAVLink identity.** A drone drops uplink frames addressed to another
  system id. A ground station reports each slot's flight-controller system id,
  flags two aircraft sharing one, and blocks commands to that id until it is
  resolved.
- Radio hops are confirmed in two phases with a relative delay, and a ground
  station that can't follow refuses the hop. A fleet ground station refuses
  drone-initiated hops.
- Python now serves only its permanent route prefixes. Video status, camera
  listing, video config, snapshots, pairing accept, the dashboard snapshot,
  Wi-Fi client scan, HLS and ground-station factory reset are native.
- `/api/status/full` drops `capabilities`, adds `fcFirmware`, and names each
  stream leg's WHEP URL `whepUrl`. The ground-station WFB view reports
  `tx_power_dbm` and the live channel (or `null`). The radio sidecars drop the
  `channel` alias in favour of `actual_channel`.
- Parameter writes use the cached parameter type (integer bytes on PX4) and
  refuse an unknown type with 409 `E_PARAM_TYPE_UNKNOWN`. Signing enrol and
  disable report whether the flight controller confirmed the change.
- The Rust workspace version now matches the agent version.
- `ados rust` is removed; nothing remains to switch.

### Fixed

- The supervisor judges the MAVLink router on frames decoded from the flight
  controller, so a wedged serial reader is restarted.
- The supervisor no longer judges any lane on process I/O, which never counts
  socket traffic. The vision engine is judged on the frames it consumed while
  an input is delivering (a fed engine used to be restarted about every 48 s),
  the swarm bus on beacons sent and heard while its radio is open, and the
  CRSF lane on RC frames written while a source is live. The CRSF stats
  sidecar gains `tx_frames_total`.
- A drone with no fleet slot no longer restart-loops the swarm bus. The bus
  exits cleanly and idles, and the supervisor starts every idle service again
  when the agent config changes, so a slot written later brings it up.
- `GET /api/video/snapshot` returns a frame from the 1 fps thumbnail stream.
  The grab skips stream probing and decoder frame threading, its deadline
  follows the encoder's live rate, and `X-Captured-At` dates the frame's
  arrival instead of the end of the grab.
- The installer installs `minisign` with its required packages and stops if
  it cannot. A direct installer run on a box without it used to skip every
  downloaded asset.
- The ground station's aux uplink recovers after its transmitter restarts. One
  failed send used to leave relayed requests, hero selection and fleet slot
  delivery failing with "Invalid argument" until the control service
  restarted. Each send after a failure now reconnects to the transmitter.
- Swarm setpoints target the learned flight-controller system id. The swarm
  replay window persists across restarts.
- A failed upgrade restores every replaced binary and restarts its units.
- Cloud commands run once even when delivered twice. WebRTC signaling is
  correlated by session and tears down its WHEP session on close.
- Status, telemetry and resource surfaces report `null` instead of zeros when
  a reading is unavailable, and the uplink stream works with the log store off.
- Hostapd, APN, uplink priority and TX power inputs are validated before they
  are applied.
- Many smaller fixes across the radio, video, networking, display, installer
  and Python setup paths.

### Removed

- The attitude-setpoint rung, the `ados-rate-control` crate, the TensorRT
  sidecar, the peripherals service, and the TCP listener of the residual
  Python API.

## [0.99.381] - 2026-09-24

### Fixed

- A plugin's `telemetry.extend` channels are kept in the published-state
  sidecar on their own budget, so a fast telemetry channel no longer evicts
  the plugin's on-change state read-backs. The first eviction of a topic is
  logged.

## [0.99.380] - 2026-09-24

### Security

- A workstation issues scoped credentials to the drones it is paired with,
  and its Atlas ingest, world stream, offload stream, artifact and job routes
  refuse any off-box caller without one. Mission Control provisions the
  credential when it holds both nodes; the drone installs it through a new
  native route and presents it on every workstation lane.
- The plugin state read refuses an id that leaves the plugin directory.
- The relay HMAC gate verifies the signature before it records the nonce, and
  refuses a non-finite timestamp.
- The ground-station beacon shared key is written owner-only and a fleet join
  never re-keys an installed key.
- The Black Box LAN query edge refuses relayed callers while unpaired, and
  ingest no longer trusts a producer's self-declared source.
- Log redaction keeps only a secret's length, in both the Rust and Python
  producers.

### Fixed

- Ground-station role transitions run in the supervisor, so switching a
  node from direct to relay can no longer stop the process doing the switch.
  `ados-wfb-rx` belongs to the direct role only, so a relay or receiver never
  runs two WFB planes on one adapter.
- A ground-station key install or unpair answers before its receive plane
  restarts.
- WFB transmit watchdogs judge each plane from its own injected-packet
  counters. Receiver and relay statistics parse the format `wfb_rx` actually
  prints, and an exited relay forwarder is respawned.
- MAVLink "not reported" sentinels are published as null on the state socket
  (state wire version 4) instead of as readings.
- The vehicle-state snapshot, mesh state, hardware rows and five sidecar
  readers stop serving values after their producer goes quiet, and treat a
  future-dated file as stale.
- Command sockets, logd queries and supervisor `systemctl` calls are bounded
  by timeouts.
- A TX-power write is persisted only once the radio confirms it.
- Recovery loops across the radio, video, installer and cloud paths retry on a
  fixed interval with no attempt cap.
- The installer enables and starts the mesh pairing daemon on ground
  stations, and apt never stops to ask about a changed config file.
- The video pipeline keeps retrying a failing encoder, drains ffmpeg's stderr
  so recording does not stall, and applies the primary leg's settings to the
  primary encoder.
- Plugin reinstalls stop the running plugin before swapping files and keep the
  working install when the new archive is refused.
- The dashboard snapshot no longer reports the configured bitrate as live and
  reads the router's real vehicle-state keys.

### Changed

- The daily plugin auto-update check runs in `ados-cloud` (Rust). `ados plugin
  check-updates` keeps the on-demand check.
- The built-in geofence, telemetry-logger and MAVLink-inspector plugins ship
  as subprocess plugins.
- Native routes: `GET/PUT /api/config`, `GET /api/logs`, `GET /api/logs/stream`,
  `GET/POST /api/wfb/pair/local-bind`, `POST /api/wfb/pair/unpair`,
  `POST /api/v1/ground-station/pair/{accept,close,approve,revoke,join}`,
  `POST /api/video/record/{start,stop}` and the observability proxy. The relay
  mesh join runs in the pairing daemon.
- New `GET /api/cloud/link` reports the broker session and the last cloud
  status POST; the dashboard's cloud panel shows both.

### Removed

- `ados demo` and the `ados-agent` entry point. The hardware-free path is the
  sim bench.
- `ados-usb-gadget.service` and the Python modules only the demo runtime and
  the retired in-process routes used.

## [0.99.379] - 2026-09-24

### Fixed

- `radio.aux_stream.send` validates the channel before narrowing it, so an
  out-of-range channel is refused instead of wrapping onto a valid one.
- `ados-mesh-pairing.service` no longer lists a sandbox path that nothing
  creates, which could stop the unit from starting. An installer test now
  checks that every required sandbox path exists or is created.

### Changed

- One WFB pair-status module serves the pair read, the auto-pair toggle and
  the pairing info route.
- Plugin install-job WebSocket tickets are scoped to a single job.
- Plugin host argument parsing and host forwards share one implementation.

## [0.99.378] - 2026-09-24

### Security

- Relayed requests are now signed over method, path and query, nonce,
  timestamp and body. A relayed config write to `security.*`,
  `server.cloud.*` or `mavlink.*` keys is refused at the edge.
- The edge rate limit is per caller and charges both native and proxied
  routes. The TCP front caps connections (512 in total, 32 per off-box caller)
  and times out a slow header read after 15 s. The PIN lockout is per caller.
- An unreadable or malformed pairing file now fails closed: claim, code and
  info answer 503. Remote callers never see the pairing code.
- Unpairing removes the relay peer secret. A tunnel request must carry a
  per-pair relay ticket naming this drone.
- Plugin sockets live in per-plugin root-owned directories. Each plugin unit
  sees only its own directory; the rest of `/run/ados` is hidden behind an
  empty read-only tmpfs.
- The plugin host checks the component id in every MAVLink frame header a
  plugin sends. Aux streams and offload sessions are owned by the plugin that
  opened them. The `plugin.` topic namespace is reserved.
- The MQTT gateway uses TLS on every transport unless
  `server.mqtt_plaintext_dev` is set, and logs in with the device id.
- The mesh relay invite is bound to the receiver, and swarm bid frames are
  checked for replay.
- Plugin archives with `.` or empty path segments, duplicate entries or an
  oversized declared size are refused before any read.
- Removed pricing and planning notes from the board definitions.

### Changed

- The router validates CRC and CRC_EXTRA on every MAVLink frame and resyncs
  on a bad one. It identifies ArduPilot firmware from the banner before
  encoding flight modes, and answers 409 when the firmware is unknown.
- A radio channel hop answers 202 with the target channel, and the radio
  command socket round trip is bounded at 5 s.
- Setting the Atlas config or a vision detector answers 502 when the service
  restart fails.
- Plugin host forwards (GPIO, video source, radio aux) are async with
  deadlines. `video.source.set` waits out a pipeline restart.
- The ground-station display shows only reported values: topology, uplink,
  video and mesh state read "unknown" until they are reported. Page ids are
  validated against the pages the running navigator publishes.
- The Rust dependency tree is updated; statically linked services no longer
  link a TLS stack.
- `ados-logd.service` no longer restarts after a clean store-disabled exit.

### Removed

- The unused `ados.security` package (certs, firewall, WireGuard, HMAC
  signing, replay), the `security.tls` and `security.wireguard` config
  blocks, and the residual Python rate-limit middleware.
- The unused swarm bid lane, and the unused MQTT gateway in `ados-cloud`.

## [0.99.377] - 2026-09-23

### Security

- Plugins granted network access are kept off the agent's own loopback
  services by an nftables rule matched on the plugin user. When the rule
  cannot be loaded, network grants are refused and the verdict is reported as
  `plugin_loopback_guard` in the full status. `nftables` is now a core
  install package.
- `GET /api/plugins/{plugin_id}/config` needs the `secret_read` MCP scope,
  since a plugin's stored config can hold a credential.

### Added

- `GET /api/plugins/{plugin_id}/config` returns a plugin's effective per-drone
  config (global keys with the drone's own keys over them) from the live
  plugin host, so a GCS can hand a plugin's UI its current settings.
- A plugin's `telemetry.extend` channels are written to its published state as
  `telemetry.<channel>`, so the GCS can serve them to that plugin's UI.
- The plugin host publishes vehicle and agent lifecycle events on the plugin
  bus.
- Changing `remote_access` settings starts or stops the tunnel unit.

### Changed

- The swarm controller re-reads its config block while running and applies a
  changed formation, spacing or participation within a few seconds. An
  engaged hard-separation latch is left alone.
- Config writes go through one store.
- The Wi-Fi client routes are served natively on every profile.
- The per-plugin config store is bounded in value size, total size and key
  count.

### Removed

- `GET`/`PUT /api/mavlink/signing/require`. No flight-controller firmware
  exposes a require-signing parameter; ArduPilot rejects unsigned commands on
  every non-USB link once it holds a key.

## [0.99.376] - 2026-09-23

### Security

- The native edge classifies each request once: on-box, lifeline, operator
  LAN or remote. A request carrying any forwarding header is remote, so a
  tunnelled caller is never treated as local. An unpaired node refuses remote
  callers, including a pairing claim.
- On a paired node the first dashboard PIN needs the node key (or an on-box
  caller). Setting a PIN by LAN presence alone works only while unpaired.
- WebSocket tickets honour MCP token scopes: a flight-class stream needs the
  flight scope.
- The MAVLink WebSocket proxy on an unpaired node admits on-box and lifeline
  callers only, and binds the configured endpoint host. On a paired node the
  raw 5760/14550 proxies serve on-box callers only unless
  `mavlink.raw_proxy_lan_access` is set. A ground-control app attached over
  the LAN to a paired node needs that key. The posture is reported as
  `mavlink_proxy_posture` in telemetry.
- Command sockets (control, radio, video, GPIO and the other command sockets,
  the log query socket and the plugin-host control socket) belong to a new
  `ados-operator` group and check peer credentials on every accept. The
  installer creates the group and adds the installing operator. Plugins are
  never in it, and plugin units hide those sockets.
- Plugin manifests are validated on every install path (id, semver version,
  entrypoints). Unit rendering refuses values with whitespace or control
  characters, and an install always lands in a direct child of the install
  directory.
- A plugin `ready_check` command runs as an argv through `systemd-run` as the
  plugin user with the plugin's sandbox, never inside the API process. HTTP
  checks must target 127.0.0.1.

### Fixed

- ArduPilot mode changes, RTL included, use the connected vehicle's mode
  table (Copter, Plane and QuadPlane, Rover). RTL on a plane used to select
  FBWB. A vehicle type with no table is refused with 409.
- Ground-station hero promotion and relay-proxy calls carry the per-drone relay
  ticket, so drones accept them. A hero selection completes even when the
  client times out, and a hero drone that reboots is promoted again.
- The swarm bus strips the radiotap FCS and drops bad-FCS frames, rebuilds its
  cipher when the fleet key changes, restarts after a bind, rejects replayed
  beacons, and reports a slot shared by two nodes (`beacons_replayed`,
  `beacons_slot_conflict`, `slot_conflict`). A stale GPS fix no longer hides
  the armed and mode state.
- The dashboard and cockpit are linted in CI.

## [0.99.374] - 2026-09-15

### Fixed

- Both pairing responses now report the resolvable system hostname. They used
  to return a name built from the device id (`ados-<6hex>.local`), which
  nothing published: an operator was handed an address that does not resolve.
  The service instance name still carries the short device id, so two nodes on
  one network stay distinguishable.
- `_ados._tcp` is advertised at boot on every profile. Without it the ground
  station's Add-a-Node discovery was always empty and the only route in was to
  type an address by hand.
- Recovery instructions no longer point at a page that was deleted. Each one
  names a command or a surface that exists.
- The relay denylist literal for factory reset did not match the route it was
  meant to block, so a factory reset was reachable over the radio relay. The
  literal and the test that pinned the wrong one are both corrected.
- The relay proxy carries response headers, so a relayed body arrives with its
  own `Content-Type` instead of being guessed at by the client.
- The cloud heartbeat carries the capability fields it was dropping, so a
  node reached over the relay is no longer a second-class node with half its
  surfaces hidden.
- Enabling a plugin now serves its socket and writes its token without a
  plugin-host restart. A freshly enabled plugin used to be inert while
  reporting success. Capability grants and revocations take effect at runtime
  for the same reason, and capability tokens rotate instead of expiring ten
  minutes into a session.
- The ten capabilities that were declared and enforced by nothing are now
  either enforced through systemd device and address-family restrictions or
  removed. A permission that grants nothing teaches an operator that the
  permission list is decorative.

### Added

- `docs/api-surface.md`, generated from the control surface's own route table,
  plus a check that every path literal in every client resolves against it. A
  client calling a route nothing serves now fails a gate instead of failing in
  the field.
- `--artifacts <dir>` installs locally built service binaries on a Linux node,
  through the same verify, atomic-replace and rollback path a released asset
  takes. Without it a bench node could not validate agent code that had not
  shipped yet, which is the one thing a bench is for. Refused on the stable
  channel, because a local build cannot carry a release signature.
- Plugin signing key enrolment, revocation distribution and downgrade
  protection, plus a host-request surface so a plugin's agent half can serve a
  request from its own GCS half.

### Changed

- `/api/video/latency` reports a stale sidecar as unavailable rather than
  serving the last number it saw as current.
- The four `/api/ota*` routes that nothing served are gone, along with the
  dashboard route that called them.

## [0.99.373] - 2026-09-15

### Fixed

- The systemd watchdog is now fed only while the monitor pass is making
  progress. It used to be pinged by an independent ticker, so a pass wedged
  inside one stage kept the unit reported healthy forever. Every `nmcli`, `ip`
  and `iw` call in the pass is also bounded by a 20 s timeout with the child
  reaped on drop, so one hung external command can no longer stall the pass
  that the watchdog now depends on.
- Parked on-demand services are retried. The retry loop skipped everything
  that was not core or hardware, so `ados-control` could latch its circuit
  open permanently and stay down until someone restarted it by hand. The
  cooldown is a fixed 5 s with no attempt cap. `ados-logd` is now supervised.
- Inbound auxiliary datagrams survive a radio-group respawn. After the first
  respawn the receive loop was left bound to the retired socket, so auxiliary
  traffic was lost permanently with no error anywhere.
- The flight-controller reconnect, ground receive and cloud bridge loops retry
  on a fixed 2-5 s interval with no attempt cap and no terminal failed state.
  Exponential backoff on a drone means an aircraft that has stopped trying.
- A camera overlay is verified on the next boot and reverted if the sensor it
  provisions does not appear. Confirmation requires evidence of the declared
  sensor, not merely of some camera, so an unrelated USB webcam cannot confirm
  a CSI overlay. The probation marker and `/run/ados/reboot-required` are read
  by something for the first time: the installer reports pending reboot
  reasons on its closing card and then performs the single reboot itself.
- `/run/ados/board.json` has a Rust writer, so a node running no Python no
  longer reports an unknown board and zero NPU capability.
- Changing the ground station's display renderer requires the renderer to be
  detected and the operator to confirm. Provisioning a boot-critical display
  overlay for hardware that is not attached is an unbootable board.

### Changed

- Video publish path: the muxer delay is stripped on every publishing branch,
  parameter sets repeat per IDR on every encode path, and the separate process
  that re-read RTSP to produce RTP is gone — one encode now fans out to the
  RTSP and RTP branches directly, which removes an RTSP session, a demux, a
  mux and a process boundary per stream.
- Ground ingest reorder window is 200 ms / 16 packets, down from 2 s / 256;
  the receiver already delivers in order, and a watchdog rather than a buffer
  is what catches a stutter.
- One mediamtx configuration generator instead of three: `ados-groundlink`
  renders it and the two Python generators are retired.
- The encoder no longer respawns for an adaptive change under 25 % of the live
  bitrate at identical geometry and frame rate, and publishes an
  `encoder_respawns` counter so a respawn storm is visible rather than
  inferred. Boards with four or more cores now get sliced threading.
- The heartbeat advertises same-origin relative video URLs the node actually
  serves, instead of instructing the ground station to build an address on a
  port the node does not proxy.
- `video.wfb.sei_latency` defaults off, matching the service that implements
  it. The two defaults disagreed, so every node with no explicit setting
  advertised a latency probe that was not running.

## [0.99.372] - 2026-09-15

### Security

- A drone that holds no per-pair relay secret now refuses relayed requests
  instead of serving them. A relayed call arrives on the drone's loopback and
  is therefore treated as on-box, so an aircraft with no credential to check
  was granting its full authority to anything holding the shared fleet radio
  key. The one path still served without a ticket is
  `POST /api/relay/peer-secret`, which is how the credential is delivered in
  the first place and is bounded by the existing first-write-wins rule; a
  refusal answers 401, and the absent-credential state is reported once per
  boot as `aux_rpc_relay_secret_absent` so it is visible in the log store.
- Plugin archive reads are bounded by total decompressed size (100 MB) and a
  per-entry expansion ratio (200x above 1 MB), matching the caps the Rust
  reader already applied. A 150 KB upload of deflated zeros previously
  expanded to 150 MB held in memory inside the API service. The archive is
  refused whole before any file is written, so an unpack cannot leave a
  half-extracted install directory behind.

## [0.99.366] - 2026-08-20

### Fixed

- Allwinner OMX hardware H.264 encode now negotiates and streams. The OMX
  GStreamer pipeline set `do-timestamp=true` on the capture source and pinned a
  `framerate` on the intermediate NV12 caps; on the A733 Cedar encoder either one
  makes `omxh264videoenc` fall back to its 176x144 default and the pipeline fails
  to preroll. Both are removed on the OMX path (the framerate flows from the
  camera's source caps), so hardware encode runs instead of falling back to
  software.

## [0.99.365] - 2026-08-20

### Fixed

- A video encoder that cannot stream on a given board no longer crash-loops
  forever with no video. If the hardware / GStreamer encoder fails to produce a
  first packet after a few restart attempts, the pipeline falls back to the
  software (ffmpeg libx264) path, which always runs — so a node can never be
  left without video by an encoder that will not start. The hardware path is
  retried fresh on the next process start.

## [0.99.364] - 2026-08-20

### Fixed

- Allwinner OMX H.264 encoder crash-loop: the OMX branch set `key-int-max`,
  which `omxh264videoenc` does not expose, so GStreamer rejected the whole
  pipeline and no video reached the media server (the camera never sustained
  capture). Use `interval-intraframes`, the gst-omx GOP property, so hardware
  H.264 encode runs on the Allwinner vendor path.
- MAC-pin duplicate address: a lone MAC randomizer derived a machine-id-only
  address that could collide with another radio's, bringing both interfaces up
  with the same MAC. The pinned MAC is now always salted by the adapter's
  stable USB path, so each radio gets a distinct, port-stable address.

## [0.99.363] - 2026-08-20

### Fixed

- The live video plane advertised absolute `:8889` / `:8888` URLs, so a browser
  off the drone LAN saw "No video source". The status surface and the per-leg
  roster built WHEP and HLS URLs from the request `Host` header, which a `.local`
  name off-LAN cannot resolve and an HTTPS ground station rejects as mixed
  content. The agent now advertises same-origin relative paths (`/whep`,
  `/whep?camera=<id>`, `/hls/main/index.m3u8`, `/hls/<id>/index.m3u8`) that
  resolve against whatever host reached the agent.
- The WHEP proxy was ground-station only, so the on-drone cockpit's own `/whep`
  returned 404. The WHEP and HLS proxy is now profile-agnostic, gained per-leg
  addressing (`?camera=<id>`) and an `/hls/<path>` passthrough, and rewrites the
  WHEP `Location` header so session teardown routes back through the proxy
  instead of the media server's loopback address.
- Two radios could be pinned the same MAC, and NetworkManager could autoconnect
  a client profile onto a WFB monitor radio at boot, before the agent set
  monitor mode. An installer step now writes a persistent `unmanaged-devices`
  rule keyed by driver, and interface roles (wfb, management wifi, mesh) are
  assigned by driver identity rather than by kernel interface name.

### Added

- Hardware H.264 encoding on Allwinner vendor (OpenMAX / Cedar) boards. A
  GStreamer `omxh264videoenc` path (constant bitrate, short keyframe interval,
  frame-dropping queue) is selected automatically when the board's HAL profile
  declares a vendor encoder API and the element is present, in place of the
  software encoder, which remains the fallback. The status surface reports
  `encoder_hw=true`.
- Operator-controllable camera image orientation. New `video.camera` (and
  per-leg) config keys `rotation` (0, 90, 180, 270), `hflip`, and `vflip`,
  applied before encode on both the ffmpeg and GStreamer paths and exposed in the
  on-drone console and the ground station settings.
- Encoder selection and keyframe-interval config knobs
  (`video.camera.encoder`, `video.camera.keyframe_interval`).

### Changed

- The cockpit video layer falls back from WHEP to HLS when WebRTC does not
  connect, so a remote viewer over a routed link still gets video. `ados status`
  resolves the relative viewer URL to the local media server.

## [0.99.361] - 2026-08-16

### Fixed

- **A drone that was streaming could report `not_initialized` with no playable
  endpoint, and a drone whose pipeline had failed could report `running` with
  one.** Two writers were racing over `/run/ados/camera-state.json`. The
  pipeline stamped its own outcome at every `start_stream` exit, and the health
  check, five seconds later and every five seconds after that, rebuilt the same
  file from camera discovery alone and reset the outcome to `unknown`. The
  status surface then had nothing to go on, so it fell through to
  `not_initialized` on a healthy node and erased the operator-facing reason on a
  failed one. There is now one writer: the health check stamps the pipeline's
  current state instead of overwriting it, a deliberate stop records `stopped`
  rather than leaving the file claiming `streaming`, and the start records
  `starting`.

- **The reader of that file trusted it forever.** Every other sidecar on the
  status surface is age-gated; this one was read with a bare filesystem read at
  a hardcoded path. When the video service died its last words stayed on the
  wire, and because the media server is a separate unit and stays bound, the
  node advertised a dialable endpoint for a pipeline that no longer existed. It
  is now resolved through the run directory and gated on the same freshness
  window as its siblings, so a stopped writer reads as `not_initialized` rather
  than as the last thing it said.

- **The camera pill said `ready` for a camera whose pipeline had failed.** Both
  the local status surface and the cloud heartbeat published the discovery
  verdict alone, so a node with a detected camera and a dead pipeline showed no
  warning at all next to an empty video pane. Discovery still decides
  `missing`; a failed pipeline now downgrades `ready` to `error`.

- **The reported encoder could not answer the question it was added for.** It
  was the internal backend name, and one of those three names covers both the
  hardware and the software H.264 paths, so "running the software fallback" and
  "running on the video engine" printed identically. The label is now derived
  from the command actually built — `rpicam-vid`, `ffmpeg-h264_v4l2m2m`,
  `ffmpeg-libx264`, `gstreamer-x264enc` and so on — and a companion
  `encoder_hw` boolean answers the hardware question directly instead of
  leaving every consumer to guess from the string.

- **A failure to build the encoder command reported the previous cycle's
  error**, which could send an operator to the media server for an encoder
  problem, and a node with no camera at all still published an encoder
  identity. Both now report what actually happened.

- **A missing `ados-video` binary retried forever.** The unit restarted it
  about twenty times a minute, indefinitely, each cycle paying a process
  spawn, a sandbox setup and three journal records, because the guard was in a
  pre-start step where the "do not restart on this exit code" directive does
  not apply. The check moved into the main command, so the unit now fails
  loudly once — journal line, marker file, `systemctl --failed` — while a real
  crash of a binary that did start still restarts as before. The pre-start
  `mkdir` was deleted with it: it could never have run, and the run directory
  is created by the tmpfiles rule anyway.

- **Two plugins subscribing to the vision stream at the same moment could
  leave both unarmed.** The arming flag was a compare-and-set, so the second
  caller saw "already arming", returned, and handed its plugin a receiver that
  would never fill if the first attempt then failed. Arming is serialized now
  and the flag is set only on success. The host also drops a pushed frame
  before decoding it when nothing is subscribed, instead of decoding and
  cloning a descriptor per frame to hand to an empty fan-out.

### Changed

- **`camera-state.json` is schema 2**: it carries the streaming pipeline's own
  outcome (`pipeline_state`, `pipeline_reason`) and the encoder identity
  (`encoder`, `encoder_hw`) alongside the discovery verdict, so a consumer can
  no longer read a detected camera as a streaming one.

- **The `/api/status/full` video block reports its basis.** It carries
  `pipeline_state` on every branch where the sidecar answered, and where the
  endpoint is being advertised on media-server readiness alone — an older agent
  that is alive and writing but does not report a pipeline state — it says so
  with `whep_url_basis: "mediamtx_readiness_only"` rather than presenting a
  guess as a fact.

- **`ados status` prints the reason and the encoder** next to the video state,
  and flags a missing `ados-video` binary when the status it is showing came
  from the agent on this machine. The marker is a local filesystem fact and was
  previously printed against remote agents too, where it described the wrong
  box.

## [0.99.360] - 2026-08-14

### Security

- **The queue that carries flight commands over the cloud relay checked only
  that a caller was signed in, not that the drone was theirs.** Knowing a device
  id was enough for any authenticated account to queue an arbitrary command at
  any node. The agent authenticates itself when it polls, and nothing further
  down the path re-checks who enqueued, so the row reached the aircraft five
  seconds later. The command name was unconstrained too, so a forged value could
  land a row the dispatcher silently ignores.

  Reading a command's status and listing a device's recent commands were open
  the same way, and an acknowledgement was accepted for any command id at all,
  so an agent holding one device's valid key could close out, and write result
  data into, another device's row. All four now resolve ownership, and the
  command name is validated against the permitted vocabulary at insert time.

- **The two raw MAVLink edges can now refuse an unauthorized peer.** TCP 5760
  and UDP 14550/14551 classified every connection and then admitted it anyway,
  logging a hardcoded `admitted = true` whatever the verdict. The classification
  is now acted on, behind `mavlink.raw_proxy_enforce_auth`.

  The flag defaults off, and that is deliberate rather than timid: these ports
  are documented as credential-free so a desktop ground station can attach, and
  the raw byte stream carries no header or subprotocol on which a caller could
  present a credential. Turning it on locks out every off-box station with no
  client-side remedy, so it is an operator's choice and not a default.

  `mavlink.aux_uplink_enforce_origin` is the companion gate one hop later, where
  an anonymous frame would otherwise reach the radio. Frames the cloud relay
  declares as off-box are exempt under both settings: that is the remote-piloting
  path for a relayed drone, and refusing it would ground the aircraft.

### Added

- **`ados network ap-passphrase` prints the unit's access-point passphrase.**
  Every surface that showed it was interactive: the OLED, the on-box panel, the
  installer's completion card, and the `ados` dashboard, which does reach a
  headless box over SSH. None of them composes, so an operator scripting a
  fleet, or just copying the key, had to read it off a screen and retype it.
  This prints the passphrase alone on stdout and nothing else, and it resolves
  the file through the platform-aware path helper rather than hardcoding the
  Linux one the dashboard assumes.

- **Config over the radio has a bearer.** The config tunnel bound its local
  ports and framed its chunks correctly, but nothing carried them to the other
  end: the crate documented a control-plane lane that is fully occupied by the
  frequency-hop handshake and has no demultiplexer. It now rides the auxiliary
  plane, which already carries several channels and already fans them out on
  both ends. A tunnel frame is at most about 145 bytes against a 1200-byte
  budget, so one chunk is one frame and there is nothing to reassemble.

- **A revision pin works on Linux.** `--ref <commit>` was accepted when building
  from source and refused on the prebuilt path, because service binaries resolve
  from rolling per-service tags and the wheel from a version tag, neither of
  which addresses a commit. Continuous integration now mirrors each run's
  byte-identical asset set to a revision-keyed release, and both halves of the
  install resolve against it, so a pin covers the Python package and the
  binaries together or fails loudly saying why.

### Fixed

- **The access point and the WiFi station find their radio by driver, not by
  name.** Three call sites still assumed `wlan0`, including a lock file whose
  name encoded the assumption. Measured across three reboots of one ground
  station, `wlan0` was the onboard chip twice and the long-range radio once, so
  the captive portal could configure itself on the flight radio. A refusal now
  also names the interface it is talking about.

- **A ground-station install no longer enables a unit it disables moments
  later.** The packaged USB-gadget and WiFi-client units were enable-linked and
  started by the same install that tore them down, because the native network
  daemon owns both in-process. The supervisor also listed three services no
  profile runs, so its report carried entries that could never be healthy.

### Removed

- **Three configuration keys that promised behaviour nothing implemented.** A
  pairing-code lifetime the agent never read, whose real 15-minute window is
  enforced by the backend that issues the code and disagreed with the agent's own
  24-hour rotation; a live-state toggle whose module was ported to Rust and now
  runs unconditionally; and a model auto-download switch with no reader, which
  nonetheless had a working toggle in the dashboard writing it.

## [0.99.359] - 2026-08-13

### Security

- **The cloud status query had no ownership check on the hosted deployment.**
  Knowing a device id was enough for any authenticated caller to read that node's
  entire status row — its addresses, its pairing state, its whole telemetry
  summary. The self-hosted Convex tree has always required ownership; the hosted
  one did not, and the exposure gate could not tell the difference because both
  sides are a plain `query`. Both trees now call the same ownership check.

### Added

- **The cloud heartbeat carries the radio block.** It was hardcoded absent, so a
  cloud-relayed node rendered an empty link card in the GCS forever while
  `/run/ados/wfb-stats.json` held the data on disk and the LAN routes read it
  fine.

  The reason it stayed absent is worth stating: the WFB status derivation existed
  in **three** copies — `/api/wfb` and `/api/status/full` each carried a private
  one, and the channel-to-frequency table a further two — so giving the cloud
  transport a block meant writing a fourth. It now lives once, in
  `ados-protocol::wfb_status`, with both LAN routes and the cloud loop calling it.
  The cloud side reads the sidecar through the same
  `read_<thing>_sidecar_from(path, now)` pattern its neighbours use, under the
  same staleness gate, and `RadioBlock::absent()` remains for the genuinely-absent
  case. A round-trip identity test pins the seam: the derivation's output must
  deserialize into `RadioBlock` and re-serialize to exactly the same JSON, so a
  field added on one side only fails rather than vanishing off the wire.

  `macStability` is carried the same way.

- **The operator audit trail has a writer.** `/var/ados/audit.jsonl` was budgeted
  by the disk janitor, trimmed on a rung, counted toward the agent's footprint and
  rendered by `ados diag` — a category an operator can see, that nothing produced.
  It now records four kinds of decision that persist beyond the process that made
  it: a regulatory posture written to config, a capability granted to a plugin, a
  sandbox rule enforced against a plugin that stepped outside it, and a
  credential-bearing pair transition. One newline-terminated JSON object per line,
  `{ts, kind, actor, detail}`. This is not a log — structured logs go to the
  logging store, which has retention and a query API. A write failure never fails
  the action being recorded.

- **Two ground-station network routes are native.**
  `PUT /api/v1/ground-station/network/client/join` and
  `DELETE /api/v1/ground-station/network/client` now reach the command socket
  directly instead of through the packaged forwarder. Pinned native route count
  150 → 152.

- **The device id of a newly-learned peer reaches the ground-station status
  route.** Two writers fired on every peer while the reconciler that consumed them
  had no callers, so the id never reached `config.yaml` and the route returned
  null across restarts.

- **Gates.** Six, each demonstrated by planting the failure it exists to catch:
  the WFB derivation and the channel table must each have exactly one definition;
  a `Mirrors <symbol>` doc citation must name a symbol that still exists; every
  path constant must have a reader somewhere; the audit record shape and its
  never-raises posture; `ados install --status` must resolve the path its own
  platform installs to; and the radio-block round trip above.

### Fixed

- **The SEI injector emitted a false start code.** Its emulation-prevention pass
  used a three-byte lookahead that could not see a forbidden pattern ending at the
  final byte, and resumed past the byte it had just escaped — between them they
  put a literal `00 00 01` into the NAL, which any decoder reads as the start of a
  new unit, not just this one's parser.

  The fix had to include the stripper. Per H.264 §7.3.1 a decoder discards the
  `0x03` of a `00 00 03` triple and resumes at the byte that followed it; ours
  consumed that byte unconditionally. While that held, "the escaper and the
  stripper are exact inverses" and "the escaper emits no false start code" could
  not both be true, so an earlier attempt confined to the escaper traded one bug
  for a worse one. Both halves are correct now, and the round trip is asserted
  across a swept input space rather than a handful of cases.

- **The ground-station status route reported recording inactive while a capture
  was running.** It hardcoded `false`, citing a sibling recorder process — but the
  recorder is held in-process by the recording route itself, behind a `OnceLock`,
  because start and stop arrive as separate requests. The route now reads it.

- **A native route used a path-parameter form the front's own matcher cannot
  read.** `:device_id` instead of `{device_id}`. Axum served it, but the route
  table classified it non-native, which moves the request off the native
  authentication lane — its rate limiter, pairing gate and scope admission — onto
  the proxied one. A test now rejects any colon segment in the table, because the
  malformed form was silent.

- **`ados install --status` read a path that cannot exist on macOS.** The command
  carried its own hardcoded literal instead of the platform-aware resolution, then
  printed that impossible path in its "nothing recorded" message.

- **`usb-rehome` and `mgmt-failover` are registered in the contracts registry.**
  Both sidecars were written and read without an entry, so nothing pinned their
  version or shape across the language boundary.

### Removed

- **The packaged ground-station network island: 27 files, 5,346 lines.** The modem
  manager, the ethernet manager, the AT-command layer, the uplink router and its
  data cap, failover, health, adapters and events, the share-uplink firewall, the
  modem and network routers, their systemd units and udev rule, and the tests that
  covered only them.

  The two routes in that tree with no native counterpart were ported first, in
  this same release, so there is no version in which they are missing.
  `wifi_client_manager.py` and `usb_gadget.py` stay: both are live.

- **Thirty-one path constants with no reader**, each left behind by a migration
  that moved its last reader into a native service. The registry is the source of
  truth for a path that crosses the language boundary; a constant that only names
  a real file is not evidence anything uses it.

- **Five encoder-identity fields in the cloud heartbeat.** `videoPipelineFlavor`,
  `videoEncoderName`, `videoEncoderHwAccel`, `videoCameraSource` and
  `videoPipelineState` were declared, defaulted to null, emitted as null and
  asserted null in the frozen fixture. No assignment of a real value existed
  anywhere; their only possible producer was the packaged air pipeline, already
  removed. Gone from the agent, from both Convex trees and from the GCS row that
  carried them to no reader.

- **`ados rust enable front` and `ados rust disable front`.** The toggle promised
  that disabling it returns the LAN port to the packaged surface. That stopped
  being true when the last packaged ground-station network writes went native
  above: the packaged surface no longer serves the routes an operator would be
  falling back to. The `front-rust-enabled` marker stays — it is the installer's
  unit-reconcile mechanism and the control surface's port selector, not a choice.
  The two systemd drop-ins the toggle wrote go with it, in the same change, so the
  flag and the code behind it never disagree. `ados rust status` still reports the
  front's mode as a reading.

### Changed

- **132 `Mirrors <symbol>` doc citations named symbols that no longer exist.**
  Each was written to document an equivalence a migration wave was maintaining,
  then outlived the predecessor it pointed at. The comment asserts that a second
  implementation exists which your edit must stay identical to, so a reader either
  hunts a file that is not there or treats the native code as a copy rather than
  the authority. The surviving citations point at the permanently-Python layers,
  and the new gate is shrink-only.

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

- **The Python encoder command builder, 814 lines.** The native encoder has
  owned the live path since the video service cut over; this module's only
  remaining reader was a hand-run script that replayed it into the byte-parity
  fixtures the Rust tests assert against. Those fixtures are committed and
  exercised by 18 of that module's 29 tests, so the reference had already been
  captured and the module was a second implementation kept only to reproduce
  numbers that are written down. The script goes with it, and the fixture file now says plainly
  that it is the reference and a new case is written by hand.

- **The `use_gst_air_pipeline` toggle.** It selected between two air-side
  pipelines, one of which no longer exists. Nothing read it, yet its default ran
  an SoC detect and a file read on every config load in every Python service, and
  it was still offered to operators in the settings schema.

- **`ados rust enable/disable display`.** The verb wrote a marker, restarted the
  display units and reported success while changing nothing: there is no packaged
  render path left to switch to, and all three units exec their native binary
  unconditionally. What the marker did do was flip the node's whole runtime badge
  to `hybrid` for as long as it sat on disk. The marker is now pruned on upgrade,
  which is what clears one already sitting on a rig.

### Fixed

- **The adaptive block of `GET /api/video/config` was always empty.** It merged
  from a sidecar with no writer, so it served `{available}` alone on every node
  while the live controller state — the chosen rung, the applied bitrate, the MCS
  and FEC pair — sat unread in `wfb-stats.json` next to it. Downstream, the
  on-device dashboard's adaptive row had been blank since the rename that
  supposedly caused it, which it did not.

  It reads the live sidecar now, behind the same freshness ceiling the `link`
  block applies to that same file: a snapshot too old to describe the radio now
  degrades to the stub rather than freezing a last-known rung beside a `link`
  block whose counters have already gone null.

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

## [0.99.357] - 2026-08-06

### Security

- **The broadcast IPC sockets are restricted to the trusted local plane.** They
  were bound world-writable (0o666) to match an older Python server, while every
  other command socket in the tree is 0o660, and two of them carry inbound bytes
  that are written straight through to the flight controller. Any local process
  could reach it.

  The mode alone was not enough. These services run as root while plugins run as
  `ados`, and 0o660 grants the group nothing until the group owns the file, so a
  socket bound that way is effectively root-only and the plugin it was created
  for cannot connect. Group ownership now lives in the shared bind helper next to
  the mode, because the two are one decision. Both stay best-effort so a host
  without the group still comes up.

### Added

- **A Hailo inference sidecar for the Pi AI HAT.** The RKNN sidecar client is now
  one `VisionBackend` carrying the accelerator name, so a single IPC path serves
  `rknn` (Rockchip/Jetson) and `hailo` (Hailo-8): the vendor runtime lives in the
  Python sidecar, not the Rust engine, so only the socket and the reported name
  differ. The HailoRT sidecar decodes `.hef` output tensors with the same
  `decode_yolo_detections` the `.rknn` and `.engine` paths use, so there is one
  tested decoder and no vendor C++ post-process, and it imports `hailo_platform`
  lazily so a host without HailoRT returns clean error responses and the engine
  falls back rather than dying. The `.hef` compile and the on-device run stay
  hardware-gated, because the compiler is x86-Linux-only.

- **A plugin can fly a flight controller that speaks MSP.** The router already
  exposed a raw MSP byte pipe on `/run/ados/msp.sock`, but nothing in the tree
  could build or parse a frame. `ados_protocol::msp` adds MSPv1 (`$M<`, XOR
  checksum) and MSPv2 (`$X<`, CRC8/DVB-S2) encode and decode, `MSP_SET_RAW_RC`,
  and the stick scaling the GCS standardised, all pinned by golden byte vectors.
  Encoding returns nothing rather than truncating an over-length payload, and
  decoding fails closed on a CRC mismatch, so a corrupt reply is never handed on
  as valid.

  On top of it: `msp.read` / `msp.write` capabilities with `msp.subscribe` /
  `msp.send`, a Python codec pinned to the identical vectors so the two halves
  cannot drift on the wire, `ctx.msp.send_sticks` (normalised roll/pitch/yaw and
  throttle, scaled and packed in AETR order, since the raw entry point applies no
  scaling and a 0.0 throttle would otherwise land at PWM 0 rather than idle), and
  a reassembler that yields whole frames, because the subscribe stream arrives in
  serial-read chunks and a decoder running once per chunk drops every frame that
  straddles two of them.

- **A gate that stops an autonomous plugin fighting a human on the controls.** An
  operator flying by hand and a plugin driving setpoints reach the flight
  controller through the same socket. A writer now declares itself an autonomous
  injector with a sticky control frame carrying its client id and an attestation
  ticket, and the router refuses that writer whenever the PIC arbiter reports a
  human holds manual control. It fails closed when the arbiter is not reporting,
  because a dead arbiter is not consent.

  It ships behind `mavlink.injector_arbitration`, default false, so the default
  posture is unchanged and every command stays on the never-gated operator path.
  The verdict is cached off the async send path: the first form read
  `pairing.json` plus the PIC sidecar and ran an HMAC verify per command, which
  at a 50-100 Hz cadence is two blocking file reads and a crypto verify per frame
  on the executor thread. The verify is now cached per sticky claim and the PIC
  read on a 50 ms TTL, so a fresh operator grab is still honoured inside that
  window.

- **The display fits any panel, and the front-panel buttons drive it.**
  Scale-to-fit rendering, configurable geometry, a virtual sink, and page
  navigation driven from the buttons rather than from a timer alone.

- **More of the plugin surface reaches Rust plugins.** GPIO, display, radio and
  tunnel senders, a real per-plugin data directory, vision detections, button
  presses and setpoints delivered inbound, and `vision.read_model` so a plugin
  receives the models resolved for it. The GCS capability mirror gained a
  self-contained drift guard.

### Fixed

- **A virtual-framebuffer reader could catch a torn frame.** The writer used a
  truncate-then-write, despite its own comment claiming the write was atomic. It
  writes a temp sibling and renames it over the target now.

- **A swapped panel or a rotation change silently mis-mapped every touch.** Touch
  calibration recorded the LCD size and rotation it was captured at and the
  loader never checked either, so a calibration from a different geometry applied
  anyway. A mismatch now falls back to the identity transform, which is what
  makes the wizard re-prompt.

- **A plugin's per-drone data directory is created before the plugin starts**,
  rather than on first write from inside it.

### Removed

- **The dead `hardware.audio` capability.** Nothing granted it and nothing
  checked it.

## [0.99.356] - 2026-08-04

### Fixed

- **A MAVLink command to a flight controller that speaks MSP is refused by
  name.** The command route built a `COMMAND_LONG` for every request. A Betaflight
  or iNav board never sends a HEARTBEAT, so the autopilot discriminator fell
  through to its absent-field default of 0 and read as ArduPilot: MAVLink bytes
  were written to the serial port of a flight controller that does not speak
  MAVLink, discarded there, while the operator was told the command was sent.

  The agent already knew. The status route surfaces both the link hint the MAVLink
  service sets when it sees MSP framing and the firmware family off the USB
  descriptor; the command route simply never consulted them. The decision is a
  pure function over the state snapshot, so both signals and the absent-field case
  are covered by tests. An absent snapshot still reads as "not known to be MSP",
  because the existing connected gate owns that case and a second refusal there
  would reject a healthy MAVLink board.

### Added

- **`--ref <commit|branch|tag>` pins the installer bootstrap to one revision.**
  `--upgrade` always pulled latest main, so a deploy that gated on one commit's CI
  could install a wheel newer than the binaries it verified. Two rigs reported one
  version while carrying binaries from an earlier one, with a fix present in the
  wheel and absent from the binary.

  A commit is fetched by object name and checked out detached, since
  `git clone --branch` never accepts a SHA, and an abbreviated SHA cannot be
  requested from a server so it is expanded against a bounded deepening of the
  branch rather than read as absent. Anything unresolvable stops the install and
  says why: falling back to main is the failure being closed.

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
