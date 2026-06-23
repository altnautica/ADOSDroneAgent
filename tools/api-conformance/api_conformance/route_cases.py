"""The route cases the harness diffs: the native-vs-Python comparison set.

Each ``RouteCase`` is one HTTP request issued identically to both transports,
plus the per-route knobs the comparison needs: the paired/unpaired header
variants, any request body, whether the response is a server-sent-event stream,
and the extra volatile fields to mask for this route on top of the default set.

The registry below is seeded with routes that are already served natively and
are safe to diff: read-only ``GET`` routes with no side effects. POST/PUT/DELETE
routes that mutate state are listed too but flagged ``require_sandbox=True`` and
skipped by default, so the registry documents the full surface without firing a
side effect against a live agent.

Adding a case as a route migrates is a one-line append to ``REGISTRY`` — the
only shared edit, mirroring the per-domain registry pattern the sibling
durable-store harness uses.
"""

from __future__ import annotations

from dataclasses import dataclass, field

# A placeholder bearer the paired-variant requests carry. The harness never
# authenticates for real; on a live run the operator supplies the agent's own
# key via the header-variant override. The default value keeps the request shape
# faithful (an Authorization header is present) without embedding a secret.
PAIRED_AUTH_PLACEHOLDER = "Bearer <api-key>"

# The live-flight telemetry leaves that move every read whenever a vehicle is
# connected: the IMU/attitude, battery, position, velocity, and GPS readings the
# FC streams, plus the telemetry-frame timestamps and the IPC drop tally. With no
# FC (e.g. a ground station) these are static zeros and compare clean; with a live
# FC they drift between the two separate reads, so the routes carrying the vehicle
# snapshot mask them. The stable scalars (armed, mode, mav_type, the param cache)
# are left to compare — the structural contract is what the two handlers must
# agree on. Scoped to the telemetry-bearing cases (not global) because some of
# these names are generic; the FC param keys are uppercase and never collide.
_TELEMETRY_VOLATILE: tuple[str, ...] = (
    # attitude
    "pitch",
    "roll",
    "yaw",
    "rollspeed",
    "pitchspeed",
    "yawspeed",
    # battery
    "voltage",
    "current",
    "cell_voltages",
    "remaining",
    "temperature",
    # position
    "lat",
    "lon",
    "alt",
    "alt_rel",
    "relative_alt",
    "heading",
    # velocity
    "vx",
    "vy",
    "vz",
    "groundspeed",
    "airspeed",
    "climb",
    # gps
    "eph",
    "epv",
    "fix_type",
    "satellites",
    # telemetry-frame timestamps + IPC drop tally
    "last_heartbeat",
    "last_update",
    "ipc_mavlink_drops",
)


@dataclass(frozen=True)
class RouteCase:
    """One route under conformance: the request to issue to both transports.

    * ``name`` — the stable identifier used by the ``--route`` filter and the
      report.
    * ``method`` / ``path`` — the HTTP verb and path issued verbatim to both.
    * ``paired_headers`` / ``unpaired_headers`` — the two auth variants. A route
      that behaves the same paired or not uses only ``unpaired_headers``; a route
      whose body depends on pairing supplies both and the harness diffs each
      variant.
    * ``body`` / ``content_type`` — the request payload for a write route (unused
      by the read routes seeded here).
    * ``is_sse`` — true when the response is a server-sent-event stream, compared
      as a frame sequence rather than a single body.
    * ``require_sandbox`` — true for a route with side effects (a write); skipped
      by default so the harness never mutates a live agent unless explicitly
      opted in.
    * ``extra_volatile`` — field keys to mask for this route on top of the
      default volatile set (e.g. a route-specific monotonic counter).
    """

    name: str
    method: str
    path: str
    unpaired_headers: dict[str, str] = field(default_factory=dict)
    paired_headers: dict[str, str] | None = None
    body: bytes | None = None
    content_type: str | None = None
    is_sse: bool = False
    require_sandbox: bool = False
    extra_volatile: tuple[str, ...] = ()


# The ordered registry. Seeded with the read-only routes already native and safe
# to diff; write routes are present but sandboxed (skipped by default). Append a
# line here as each route migrates.
REGISTRY: list[RouteCase] = [
    # Liveness: the smallest possible body, the first thing a native front
    # serves. No auth, no volatile fields beyond an optional uptime.
    RouteCase(
        name="healthz",
        method="GET",
        path="/healthz",
    ),
    # The agent version string. Stable between two reads of the same build.
    RouteCase(
        name="version",
        method="GET",
        path="/api/version",
    ),
    # The current time the agent reports. Entirely volatile, so this case proves
    # the masking: both bodies collapse to the sentinel and compare equal.
    RouteCase(
        name="time",
        method="GET",
        path="/api/time",
    ),
    # The composite status body. Carries timestamps and counters masked by the
    # default volatile set; the rest (profile, capabilities, link state) is the
    # contract the two handlers must agree on. The nested health block's live
    # system readings (cpu/temperature/memory/disk) move every read, so they are
    # masked too.
    RouteCase(
        name="status",
        method="GET",
        path="/api/status",
        paired_headers={"authorization": PAIRED_AUTH_PLACEHOLDER},
        extra_volatile=(
            "cpu_percent",
            "temperature",
            "memory_percent",
            "disk_percent",
        ),
    ),
    # The live telemetry snapshot. The numeric values move every read, so the
    # case masks the snapshot's own timestamp and leaves the shape to compare.
    RouteCase(
        name="telemetry",
        method="GET",
        path="/api/telemetry",
        paired_headers={"authorization": PAIRED_AUTH_PLACEHOLDER},
        extra_volatile=_TELEMETRY_VOLATILE,
    ),
    # Pairing info: what an unpaired agent advertises to a claimer on the LAN.
    # Read freely while unpaired (being on the LAN is the auth boundary).
    RouteCase(
        name="pairing-info",
        method="GET",
        path="/api/pairing/info",
    ),
    # The short-lived pairing code an unpaired agent shows. The code itself is
    # regenerated, so it is masked as a route-extra volatile field.
    RouteCase(
        name="pairing-code",
        method="GET",
        path="/api/pairing/code",
        extra_volatile=("code", "expires_at", "expires_in"),
    ),
    # The queued command list the agent polls. Empty on an idle agent; the entry
    # ids and enqueue timestamps are volatile when present.
    RouteCase(
        name="commands",
        method="GET",
        path="/api/commands",
        paired_headers={"authorization": PAIRED_AUTH_PLACEHOLDER},
        extra_volatile=("id", "enqueued_at"),
    ),
    # Fleet enrollment: the opt-in mesh awareness flag. Static not-enrolled
    # object on this device, identical paired or not, no volatile fields.
    RouteCase(
        name="fleet-enrollment",
        method="GET",
        path="/api/fleet/enrollment",
    ),
    # Fleet peers: the roster the agent has discovered. Empty list with
    # enrollment off — a bare array, the steady-state response.
    RouteCase(
        name="fleet-peers",
        method="GET",
        path="/api/fleet/peers",
    ),
    # The full cached FC parameter list + the sweep-progress envelope. The values
    # and the cached/expected counts move as the sweep fills the cache, so they
    # are masked; the envelope shape (params/count/cached/priming flags/progress)
    # is the contract.
    RouteCase(
        name="params",
        method="GET",
        path="/api/params",
        paired_headers={"authorization": PAIRED_AUTH_PLACEHOLDER},
        extra_volatile=("count", "cached", "got", "expected"),
    ),
    # The live `ados-*.service` unit inventory. The per-service memory and the
    # serving process's own pid/cpu/memory move between reads, so they are masked;
    # the unit shape (name/state/sub_state/load_state/active) is the contract.
    RouteCase(
        name="services",
        method="GET",
        path="/api/services",
        paired_headers={"authorization": PAIRED_AUTH_PLACEHOLDER},
        extra_volatile=("memory_mb", "pid", "cpu_percent"),
    ),
    # MAVLink signing capability: whether the connected FC supports v2 signing.
    # Read from the snapshot's FC flag + autopilot id + param tree; stable shape.
    RouteCase(
        name="signing-capability",
        method="GET",
        path="/api/mavlink/signing/capability",
    ),
    # The current SIGNING_REQUIRE param value from the cached param blob. Stable.
    RouteCase(
        name="signing-require",
        method="GET",
        path="/api/mavlink/signing/require",
    ),
    # The observational signed-frame counters. The last-signed timestamp moves
    # when an observer is present, so it is masked.
    RouteCase(
        name="signing-counters",
        method="GET",
        path="/api/mavlink/signing/counters",
        extra_volatile=("last_signed_rx_at",),
    ),
    # WFB link status: state, channel, adapter, and the link-quality numbers. The
    # signal/packet fields move every read, so they are masked; the shape +
    # channel-derived frequency/bandwidth + adapter block are the contract.
    RouteCase(
        name="wfb",
        method="GET",
        path="/api/wfb",
        extra_volatile=(
            "rssi_dbm",
            "noise_dbm",
            "snr_db",
            "packets_received",
            "packets_lost",
            "loss_percent",
            "fec_recovered",
            "fec_failed",
            "bitrate_kbps",
            "bitrate_mbps",
            "rx_silent_seconds",
            "restart_count",
            # Per-second rate counters off the live link, recomputed every read.
            "valid_rx_packets_per_s",
            "video_inbound_bytes_per_s",
            "samples",
            "state",
        ),
    ),
    # WFB link-quality history: a list of per-bucket samples. The timestamps + the
    # numeric readings move every read, so they are masked; the {samples, count}
    # shape is the contract.
    RouteCase(
        name="wfb-history",
        method="GET",
        path="/api/wfb/history",
        extra_volatile=(
            "samples",
            "count",
            "timestamp",
            "rssi_dbm",
            "snr_db",
            "loss_percent",
            "bitrate_kbps",
        ),
    ),
    # WFB pair-state snapshot: paired flag, peer device-id, fingerprint, auto-pair,
    # role. Stable between two reads (the fingerprint is a digest of the on-disk
    # key, the peer/role come off config).
    RouteCase(
        name="wfb-pair",
        method="GET",
        path="/api/wfb/pair",
    ),
    # WFB local-bind to cloud-relay failover state. A single {failover_state}
    # field, stable between two reads of the same agent.
    RouteCase(
        name="wfb-pair-failover-status",
        method="GET",
        path="/api/wfb/pair/failover-status",
    ),
    # The consolidated status: agent info, services, resources, video, telemetry,
    # radio, and mesh in one body. The numeric resource/health readings, the
    # per-service memory, and the live radio link metrics move every read, so they
    # are masked (in both the snake-case resource/health spelling and the camelCase
    # radio-block spelling); the structural contract (the block keys, profile/role,
    # runtimeMode, video state, mesh shape) is what the two handlers must agree on.
    RouteCase(
        name="status-full",
        method="GET",
        path="/api/status/full",
        paired_headers={"authorization": PAIRED_AUTH_PLACEHOLDER},
        extra_volatile=(
            # Health + resources readings (snake-case).
            "cpu_percent",
            "memory_percent",
            "memory_used_mb",
            "memory_available_mb",
            "memory_cache_mb",
            "memory_total_mb",
            "swap_used_mb",
            "swap_total_mb",
            "swap_percent",
            "disk_percent",
            "disk_used_gb",
            "disk_total_gb",
            "temperature",
            # Per-service grouped memory.
            "memory_mb",
            # Live radio link metrics (camelCase, post-remap).
            "rssiDbm",
            "snrDb",
            "noiseDbm",
            "bitrateKbps",
            "fecRecovered",
            "fecLost",
            "packetsLost",
            "lossPercent",
            "rxSilentSeconds",
            "restartCount",
            "reacquireKills",
            "validRxPacketsPerS",
            "txBytesPerS",
            "txZombieKills",
            "txVideoRecvqBytes",
            "txVideoStallKills",
            "adapterUsbSpeedMbps",
            # The radio + WFB-receive link state can flip between two reads.
            "state",
            # The video WHEP URL is liveness-derived (present only while the
            # downlink delivers): the native front reads the WFB link from the
            # buffered logging store while the residual reads it live, so the
            # delivering boolean — keyed on the per-instant valid-decode rate —
            # can flip between the two reads, exactly like the `state` above it.
            "whep_url",
            # Camera USB-recovery attempt counters.
            "attempts",
            "maxAttempts",
        )
        + _TELEMETRY_VOLATILE,
    ),
    # Video glass-to-glass latency. The latency / ewma / pipeline readings and the
    # sample count move every read (the SEI probe keeps sampling), so they are
    # masked; the {latency_ms, ewma_ms, pipeline_latency_ms, samples, source} shape
    # is the contract.
    RouteCase(
        name="video-latency",
        method="GET",
        path="/api/video/latency",
        extra_volatile=(
            "latency_ms",
            "ewma_ms",
            "pipeline_latency_ms",
            "samples",
        ),
    ),
    # Air-side pipeline stats snapshot. The counters / gauges / monotonic floats all
    # move every read, so they are masked; the field set (camera_source /
    # encoder_name / pipeline_state + the counters + the three live-only floats) is
    # the contract. A 204 (pipeline not in use) compares as an empty body on both.
    RouteCase(
        name="video-air-pipeline",
        method="GET",
        path="/api/v1/video/air-pipeline",
        extra_volatile=(
            "sei_injected_count",
            "udp_bytes_out",
            "restart_count",
            "tx_silent_kicks",
            "bus_errors",
            "updated_at_ms",
            "encoder_fps",
            "encoded_kbps",
            "started_at",
            "last_state_change_at",
            "last_buffer_at",
        ),
    ),
    # Video encoder + radio config snapshot. The dynamic adaptive / hopping / link
    # blocks carry live link readings + hop history that move between reads, so those
    # are masked; the static radio + encoder blocks (channel/band/mcs/fec/codec/…)
    # are the contract.
    RouteCase(
        name="video-config",
        method="GET",
        path="/api/video/config",
        extra_volatile=(
            "tx_bytes_per_s",
            "valid_rx_packets_per_s",
            "video_inbound_bytes_per_s",
            "rx_silent_seconds",
            "channel_locked",
            "acquire_state",
            "history",
            "last_hop_at",
        ),
    ),
    # Ground-station composite status: the OLED-aligned snapshot. On a drone
    # profile both transports answer 404 E_PROFILE_MISMATCH (identical); on a
    # ground station the structural blocks (profile, role, gcs, video) are the
    # contract while the live system + link readings move every read, so those keys
    # are masked.
    RouteCase(
        name="gs-status",
        method="GET",
        path="/api/v1/ground-station/status",
        paired_headers={"authorization": PAIRED_AUTH_PLACEHOLDER},
        extra_volatile=(
            "cpu_pct",
            "ram_used_mb",
            "ram_total_mb",
            "temp_c",
            "agent_version",
            "rssi_dbm",
            "bitrate_mbps",
            "bitrate_kbps",
            "snr_db",
            "noise_dbm",
            "packets_received",
            "packets_lost",
            "loss_percent",
            "fec_recovered",
            "fec_lost",
            "fec_failed",
            "state",
        ),
    ),
    # Ground-station stored radio config {channel, bitrate_profile, fec}. Stable
    # between two reads (it comes off the on-disk config). 404 off a drone profile.
    RouteCase(
        name="gs-wfb",
        method="GET",
        path="/api/v1/ground-station/wfb",
    ),
    # Relay-role fragment counters. 404 E_WRONG_ROLE off a relay node (identical on
    # both transports); on a relay the fragment counters advance between reads.
    RouteCase(
        name="gs-wfb-relay-status",
        method="GET",
        path="/api/v1/ground-station/wfb/relay/status",
        extra_volatile=(
            "fragments",
            "fragments_received",
            "fragments_forwarded",
            "bytes_forwarded",
            "receiver_reachable",
        ),
    ),
    # Receiver-role per-relay fragment counters. 404 off a receiver node; on a
    # receiver the per-relay counters advance between reads.
    RouteCase(
        name="gs-wfb-receiver-relays",
        method="GET",
        path="/api/v1/ground-station/wfb/receiver/relays",
        extra_volatile=(
            "fragments",
            "fragments_received",
            "last_seen",
        ),
    ),
    # Receiver-role combined FEC output stats. 404 off a receiver node; on a
    # receiver the output counters advance between reads (only `up` is stable).
    RouteCase(
        name="gs-wfb-receiver-combined",
        method="GET",
        path="/api/v1/ground-station/wfb/receiver/combined",
        extra_volatile=(
            "fragments_after_dedup",
            "fec_repaired",
            "output_kbps",
        ),
    ),
    # Ground-station batman-adv state snapshot. 404 with E_NOT_IN_MESH on a direct
    # node (both transports), otherwise the live snapshot whose neighbor/gateway
    # readings move every read, so they are masked; the snapshot shape is the
    # contract.
    RouteCase(
        name="gs-mesh",
        method="GET",
        path="/api/v1/ground-station/mesh",
        extra_volatile=("neighbors", "gateways", "selected", "selected_gateway"),
    ),
    # Ground-station mesh neighbors slice. Same gates; the neighbor list moves, so
    # it is masked.
    RouteCase(
        name="gs-mesh-neighbors",
        method="GET",
        path="/api/v1/ground-station/mesh/neighbors",
        extra_volatile=("neighbors",),
    ),
    # Ground-station mesh routes slice (aliased to neighbors). Same gates; the
    # route list moves, so it is masked.
    RouteCase(
        name="gs-mesh-routes",
        method="GET",
        path="/api/v1/ground-station/mesh/routes",
        extra_volatile=("routes",),
    ),
    # Ground-station mesh gateways slice. Same gates; the gateway list + the
    # selected gateway move, so they are masked.
    RouteCase(
        name="gs-mesh-gateways",
        method="GET",
        path="/api/v1/ground-station/mesh/gateways",
        extra_volatile=("gateways", "selected"),
    ),
    # Ground-station aggregate uplink view: ap / wifi_client / ethernet /
    # modem_4g / active_uplink / priority / share_uplink. On a drone profile both
    # transports return the same E_PROFILE_MISMATCH 404; on a ground station the
    # shape is the contract. The live link/usage readings move every read, so the
    # signal/usage/uplink fields are masked.
    RouteCase(
        name="gs-network",
        method="GET",
        path="/api/v1/ground-station/network",
        extra_volatile=(
            "signal",
            "data_used_mb",
            "percent",
            "active_uplink",
            "rssi_dbm",
            "rssi_pct",
            "connected",
            "ip",
            "current_ip",
            "current_gateway",
            "gateway",
            "link",
            "speed_mbps",
            "dhcp_lease_remaining_s",
        ),
    ),
    # Ground-station nearby-network scan. A {networks} list; the scan results move
    # every read, so the list is masked — the {networks} shape is the contract.
    RouteCase(
        name="gs-network-client-scan",
        method="GET",
        path="/api/v1/ground-station/network/client/scan",
        extra_volatile=("networks",),
    ),
    # Ground-station uplink priority list. A {priority} list, stable between two
    # reads of the same agent (it is the persisted config order).
    RouteCase(
        name="gs-network-priority",
        method="GET",
        path="/api/v1/ground-station/network/priority",
    ),
    # Ground-station cellular detail snapshot. present + the live signal readings;
    # the readings move every read, so they are masked — present/reason is the
    # contract.
    RouteCase(
        name="gs-modem-status",
        method="GET",
        path="/api/v1/ground-station/modem-status",
        extra_volatile=(
            "rssi_pct",
            "rssi_dbm",
            "rsrp_dbm",
            "rsrq_db",
            "sinr_db",
            "ip",
            "operator",
            "tech",
            "band",
        ),
    ),
    # Ground-station mesh pairing snapshot. Profile-gated: a drone node returns
    # the nested E_PROFILE_MISMATCH 404 on both transports; a ground station with
    # no Accept window open returns the steady-state {"open": false}. No volatile
    # fields in either steady state.
    RouteCase(
        name="gs-pair-pending",
        method="GET",
        path="/api/v1/ground-station/pair/pending",
    ),
    # Ground-station PIC arbiter state. Profile-gated; on a ground station the
    # arbiter starts unclaimed, so both transports report the same unclaimed
    # default. The claim counter + session timestamp + primary gamepad move once a
    # client claims PIC, so they are masked.
    RouteCase(
        name="gs-pic",
        method="GET",
        path="/api/v1/ground-station/pic",
        extra_volatile=("claim_counter", "claimed_since", "primary_gamepad_id"),
    ),
    # Ground-station captive-portal token mint. Profile-gated; on a ground station
    # reached on-box (loopback) it mints a fresh single-use token. The token value
    # is regenerated every call, so it is masked; the {token} shape is the contract.
    RouteCase(
        name="gs-captive-token",
        method="GET",
        path="/api/v1/ground-station/captive-token",
        extra_volatile=("token",),
    ),
    # Write a single FC parameter. A side effect (it sends a PARAM_SET to the FC),
    # so it is sandboxed and skipped by default; the bench opts in against a live
    # FC. The body is the {"value": <number>} the route reshapes into the frame.
    # The `ack` flag and `cached_value` depend on whether the FC echoes the new
    # value within the poll window, so they are masked; the {name, value, message}
    # shape is the contract.
    RouteCase(
        name="params-write",
        method="POST",
        path="/api/params/WPNAV_SPEED",
        body=b'{"value": 500.0}',
        content_type="application/json",
        paired_headers={"authorization": PAIRED_AUTH_PLACEHOLDER},
        require_sandbox=True,
        extra_volatile=("ack", "cached_value", "message"),
    ),
    # Push a 32-byte signing key to the FC (one-shot SETUP_SIGNING). A side effect
    # (it writes to the FC), so sandboxed by default. The body is a 64-hex-char
    # key + the target/link fields; the response carries a volatile `enrolled_at`
    # timestamp + a per-key `key_id` fingerprint, masked when the bench opts in.
    RouteCase(
        name="signing-enroll-fc",
        method="POST",
        path="/api/mavlink/signing/enroll-fc",
        paired_headers={"authorization": PAIRED_AUTH_PLACEHOLDER},
        body=(
            b'{"key_hex":'
            b'"0000000000000000000000000000000000000000000000000000000000000000",'
            b'"link_id":0,"target_system":1,"target_component":1}'
        ),
        content_type="application/json",
        require_sandbox=True,
        extra_volatile=("enrolled_at", "key_id"),
    ),
    # Clear the FC's signing store (SETUP_SIGNING with an all-zero key). A side
    # effect, so sandboxed. No request body; the response is the static
    # {"success": true}.
    RouteCase(
        name="signing-disable-on-fc",
        method="POST",
        path="/api/mavlink/signing/disable-on-fc",
        paired_headers={"authorization": PAIRED_AUTH_PLACEHOLDER},
        require_sandbox=True,
    ),
    # Toggle SIGNING_REQUIRE on the FC (PARAM_SET). A side effect, so sandboxed.
    # The body is the {require} flag; the response echoes it in
    # {"success": true, "require": <bool>}.
    RouteCase(
        name="signing-require-set",
        method="PUT",
        path="/api/mavlink/signing/require",
        paired_headers={"authorization": PAIRED_AUTH_PLACEHOLDER},
        body=b'{"require":true}',
        content_type="application/json",
        require_sandbox=True,
    ),
    # Restart a single agent unit. A write with side effects, so it is sandboxed
    # (skipped by default). The path carries a concrete allowlisted unit name; the
    # body is unused by the handler (the unit comes from the path), but a
    # representative empty JSON object is sent. The success body's pid/timestamp
    # before/after fields move with the live restart, so they are masked; the
    # deterministic diff is the unknown-service rejection shape.
    RouteCase(
        name="service-restart",
        method="POST",
        path="/api/services/ados-mavlink/restart",
        body=b"{}",
        content_type="application/json",
        require_sandbox=True,
        extra_volatile=(
            "pid_before",
            "pid_after",
            "active_enter_before",
            "active_enter_after",
        ),
    ),
    # Restart the supervisor (the whole agent process tree). A write with side
    # effects, so it is sandboxed (skipped by default). The handler takes no body;
    # a representative empty JSON object is sent. The {ok, message} shape is the
    # contract.
    RouteCase(
        name="system-restart-supervisor",
        method="POST",
        path="/api/v1/system/restart-supervisor",
        body=b"{}",
        content_type="application/json",
        require_sandbox=True,
    ),
    # Join a Wi-Fi network. A side effect (it stops hostapd, transitions wlan0 to
    # STA, and waits for an IP through the uplink daemon), so sandboxed and skipped
    # by default; the bench opts in against a live wlan0. The body is the
    # {ssid, passphrase, force} the route forwards as a wifi_join op. The ip /
    # gateway move with the join outcome, so they are masked; the {joined, error}
    # shape is the contract.
    RouteCase(
        name="network-client-join",
        method="PUT",
        path="/api/v1/network/client/join",
        body=b'{"ssid": "BenchNet", "passphrase": "benchpass", "force": false}',
        content_type="application/json",
        paired_headers={"authorization": PAIRED_AUTH_PLACEHOLDER},
        require_sandbox=True,
        extra_volatile=("ip", "gateway"),
    ),
    # Disconnect the current Wi-Fi-client link. A side effect (it drops wlan0 and
    # may restore hostapd), so sandboxed. No request body; the response carries the
    # previous SSID, which depends on the live link, so it is masked; the {left}
    # shape is the contract.
    RouteCase(
        name="network-client-leave",
        method="DELETE",
        path="/api/v1/network/client",
        paired_headers={"authorization": PAIRED_AUTH_PLACEHOLDER},
        require_sandbox=True,
        extra_volatile=("previous_ssid",),
    ),
    # Change the WFB-ng channel. A side effect (it forwards a coordinated hop to
    # the radio command socket), so sandboxed and skipped by default; the bench
    # opts in against a live radio. The body is the {"channel": N} the route
    # validates and forwards. The success body is the static
    # {status, channel, frequency_mhz} shape (the echoed channel is the radio's,
    # so it is masked); the deterministic diff is the invalid-channel rejection.
    RouteCase(
        name="wfb-channel-write",
        method="POST",
        path="/api/wfb/channel",
        body=b'{"channel": 149}',
        content_type="application/json",
        paired_headers={"authorization": PAIRED_AUTH_PLACEHOLDER},
        require_sandbox=True,
        extra_volatile=("channel",),
    ),
    # Set the WFB-ng TX power at runtime. A side effect (it forwards to the radio
    # command socket + persists to config), so sandboxed by default. The body is
    # the {"tx_power_dbm": N} the route bounds and forwards; the response carries
    # an `effective_dbm` the driver reports (volatile, masked), and the
    # deterministic {requested_dbm, tx_power_max_dbm} legs are the contract.
    RouteCase(
        name="wfb-tx-power-write",
        method="PUT",
        path="/api/wfb/tx-power",
        body=b'{"tx_power_dbm": 10}',
        content_type="application/json",
        paired_headers={"authorization": PAIRED_AUTH_PLACEHOLDER},
        require_sandbox=True,
        extra_volatile=("effective_dbm",),
    ),
    # Set the ground-station uplink priority list. A write with side effects (it
    # persists the priority file the failover daemon reads), so it is sandboxed
    # and skipped by default; the bench opts in on a ground-station rig. The body
    # is the {"priority": [...]} list; the response echoes the persisted order in
    # {"priority": [...]}. Profile-gated: a drone node returns the nested
    # E_PROFILE_MISMATCH 404 on both transports.
    RouteCase(
        name="gs-network-priority-set",
        method="PUT",
        path="/api/v1/ground-station/network/priority",
        body=b'{"priority": ["eth0", "wlan0_client", "wwan0", "usb0"]}',
        content_type="application/json",
        require_sandbox=True,
    ),
    # Pin a stable MAC for an adapter. A write (it merges the mac_pin config the
    # supervisor reconciler reads + removes the stale .link), so sandboxed and
    # skipped by default. The body is the {iface, mac}; the response is the
    # {status, iface, mac, persisted, appliedLive, note} shape. The `note` text
    # varies with the box's live default-route/mgmt-iface state, so it is masked.
    RouteCase(
        name="mac-pin",
        method="POST",
        path="/api/v1/network/mac/pin",
        body=b'{"iface": "wlan0", "mac": "02:c6:75:83:1a:3e"}',
        content_type="application/json",
        paired_headers={"authorization": PAIRED_AUTH_PLACEHOLDER},
        require_sandbox=True,
        extra_volatile=("note",),
    ),
    # Clear a MAC pin for an adapter. A write (it pops the override + removes the
    # .link), so sandboxed. The {iface} segment carries a representative adapter;
    # the response is the static {status, iface, removedOverride, removedLinkFile,
    # note} shape with no volatile fields.
    RouteCase(
        name="mac-unpin",
        method="DELETE",
        path="/api/v1/network/mac/wlan0",
        paired_headers={"authorization": PAIRED_AUTH_PLACEHOLDER},
        require_sandbox=True,
    ),
    # Set the ground-station WFB radio config. A write (a surgical video.wfb config
    # merge the radio/ground services read on their cadence), so sandboxed and
    # skipped by default; the bench opts in on a ground-station rig. The body is the
    # {channel, bitrate_profile, fec}; the deterministic {channel, bitrate_profile,
    # fec} radio view is the contract. `persisted`/`persist_error` depend on the
    # writer's euid (a non-root front vs the root FastAPI), so they are masked.
    # Profile-gated: a drone node returns the same E_PROFILE_MISMATCH 404 on both.
    RouteCase(
        name="gs-wfb-config-set",
        method="PUT",
        path="/api/v1/ground-station/wfb",
        body=b'{"channel": 161, "bitrate_profile": "high", "fec": "8/12"}',
        content_type="application/json",
        require_sandbox=True,
        extra_volatile=("persisted", "persist_error"),
    ),
    # Toggle the WFB auto-pair arm flag. A write (it merges video.wfb.auto_pair_enabled
    # into the config after reading the live pair status), so sandboxed and skipped by
    # default; the bench opts in. The body is {"enabled": false} (disable is never
    # re-arm-blocked, so both transports take the persist path). The response is the
    # pair-status snapshot {paired, paired_with_device_id, paired_at, fingerprint,
    # auto_pair_enabled, role}; the per-pair-state fields move with whatever the rig is
    # bound to, so they are masked, and the {auto_pair_enabled, role} legs are the
    # contract. `rearm_blocked` would appear only on the enabled=true paired path.
    RouteCase(
        name="wfb-auto-pair-set",
        method="PUT",
        path="/api/wfb/pair/auto-pair",
        body=b'{"enabled": false}',
        content_type="application/json",
        paired_headers={"authorization": PAIRED_AUTH_PLACEHOLDER},
        require_sandbox=True,
        extra_volatile=(
            "paired",
            "paired_with_device_id",
            "paired_at",
            "fingerprint",
        ),
    ),
    # Trigger an operator cloud-export of a log window. A write (it writes the
    # push-request file the cloud service consumes), so sandboxed and skipped by
    # default. The body is {"wait": false} so both transports return the immediate 202
    # pending placeholder without blocking on the poll. The `request_id` is a fresh
    # uuid per call, so it is masked; the {accepted, pushed, deduped, bytes, rows,
    # synced, error, pending} placeholder shape is the contract.
    RouteCase(
        name="logs-push",
        method="POST",
        path="/api/logs/push",
        body=b'{"wait": false}',
        content_type="application/json",
        paired_headers={"authorization": PAIRED_AUTH_PLACEHOLDER},
        require_sandbox=True,
        extra_volatile=("request_id",),
    ),
    # (The /api/system + /api/v1/diagnostics reads finished migration: native is
    # the sole server, the residual no longer serves them, so their conformance
    # cases retired with the Python prune — a diff against an absent residual would
    # be a false failure. The native goldens in ados-control pin their shape now.)
    # Ground-station recordings listing {recording, current_filename, items[]}.
    # 404 E_PROFILE_MISMATCH off a drone (identical both transports); on a GS the
    # envelope is the contract and the per-file rows are volatile (a fresh GS with
    # no recordings compares clean with an empty list).
    RouteCase(
        name="gs-recording-list",
        method="GET",
        path="/api/v1/ground-station/recording/list",
        paired_headers={"authorization": PAIRED_AUTH_PLACEHOLDER},
        extra_volatile=(
            "items",
            "filename",
            "size_bytes",
            "mtime",
            "current_filename",
        ),
    ),
    # Ground-station persisted UI config {oled, buttons, screens}: the on-disk
    # side-file merged over the defaults. Stable between two reads; 404 off a
    # drone profile.
    RouteCase(
        name="gs-ui",
        method="GET",
        path="/api/v1/ground-station/ui",
    ),
    # Ground-station HDMI kiosk display config {resolution, kiosk_enabled,
    # kiosk_target_url}. Stable; 404 off a drone profile.
    RouteCase(
        name="gs-display",
        method="GET",
        path="/api/v1/ground-station/display",
    ),
    # Ground-station attached controllers + primary selection. 404 off a drone;
    # on a GS the {devices, primary_id} shape is the contract while the list +
    # persisted primary depend on what is plugged in, so both are masked.
    RouteCase(
        name="gs-gamepads",
        method="GET",
        path="/api/v1/ground-station/gamepads",
        extra_volatile=("devices", "primary_id"),
    ),
    # Ground-station paired Bluetooth devices. 404 off a drone; on a GS the
    # {devices} envelope is the contract while the paired list is masked.
    RouteCase(
        name="gs-bluetooth-paired",
        method="GET",
        path="/api/v1/ground-station/bluetooth/paired",
        extra_volatile=("devices",),
    ),
    # CAN passthrough: a deliberate 501 stub with a fixed {error, message}
    # envelope and no side effect, so it is safe to fire against both transports
    # and is NOT sandboxed. Both return the same fixed body, no volatile fields.
    RouteCase(
        name="can-passthrough",
        method="POST",
        path="/api/can/passthrough",
    ),
    # A single FC parameter by name. The path carries a synthetic name no FC
    # parameter ever uses, so both transports take the not-found path and return
    # the byte-identical 404 {"detail": ...}. No volatile fields.
    RouteCase(
        name="params-single",
        method="GET",
        path="/api/params/__ADOS_CONFORMANCE_PROBE__",
        paired_headers={"authorization": PAIRED_AUTH_PLACEHOLDER},
    ),
    # Per-adapter stable-MAC verdicts read from the on-disk state file. Profile
    # agnostic (no gate); a board with no tracked adapters returns
    # {"version": 1, "adapters": []}. The {version, adapters} envelope is the
    # contract; the live-observed verdict fields move when the reconciler
    # rewrites the file between the two reads, so they are masked.
    RouteCase(
        name="mac-adapters",
        method="GET",
        path="/api/v1/network/mac/adapters",
        extra_volatile=(
            "adapters",
            "lastSeenMac",
            "appliedLive",
            "state",
            "source",
            "deferredReason",
        ),
    ),
    # The live Wi-Fi-client station status off the uplink daemon's command
    # socket. The link readings move with the live link, so they are masked; the
    # {connected, ...} envelope is the contract.
    RouteCase(
        name="network-client-status",
        method="GET",
        path="/api/v1/network/client/status",
        paired_headers={"authorization": PAIRED_AUTH_PLACEHOLDER},
        extra_volatile=(
            "connected",
            "ssid",
            "bssid",
            "signal",
            "ip",
            "gateway",
            "security",
        ),
    ),
    # <append a RouteCase line per route as it migrates — the only shared edit>
]


def registered_cases() -> list[RouteCase]:
    """Every registered route case, in report order."""
    return list(REGISTRY)


def case_by_name(name: str) -> RouteCase | None:
    """Look up one route case by name (for the ``--route`` CLI filter)."""
    for case in REGISTRY:
        if case.name == name:
            return case
    return None
