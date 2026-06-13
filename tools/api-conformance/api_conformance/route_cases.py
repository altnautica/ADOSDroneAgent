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
    # contract the two handlers must agree on.
    RouteCase(
        name="status",
        method="GET",
        path="/api/status",
        paired_headers={"authorization": PAIRED_AUTH_PLACEHOLDER},
    ),
    # The live telemetry snapshot. The numeric values move every read, so the
    # case masks the snapshot's own timestamp and leaves the shape to compare.
    RouteCase(
        name="telemetry",
        method="GET",
        path="/api/telemetry",
        paired_headers={"authorization": PAIRED_AUTH_PLACEHOLDER},
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
            # Camera USB-recovery attempt counters.
            "attempts",
            "maxAttempts",
        ),
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
    # Ground-station mesh role: the current role, the configured role, the
    # supported list, the role's systemd units, and the full mesh-unit set. On a
    # ground station the body is stable; on a drone both transports 404 with the
    # same E_PROFILE_MISMATCH body, so the case compares equal either way.
    RouteCase(
        name="gs-role",
        method="GET",
        path="/api/v1/ground-station/role",
    ),
    # Ground-station mesh config: the configured mesh transport fields off the
    # agent config. Stable between two reads of the same agent.
    RouteCase(
        name="gs-mesh-config",
        method="GET",
        path="/api/v1/ground-station/mesh/config",
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
    # Ground-station ethernet profile + live link. The mode + static fields are
    # the contract; the live link/IP move, so they are masked.
    RouteCase(
        name="gs-network-ethernet",
        method="GET",
        path="/api/v1/ground-station/network/ethernet",
        extra_volatile=(
            "link",
            "speed_mbps",
            "current_ip",
            "current_gateway",
            "ip",
            "gateway",
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
    # Ground-station modem view: enabled / apn / cap + cumulative usage. The
    # connectivity + usage readings move every read, so they are masked.
    RouteCase(
        name="gs-network-modem",
        method="GET",
        path="/api/v1/ground-station/network/modem",
        extra_volatile=(
            "connected",
            "iface",
            "ip",
            "signal_quality",
            "technology",
            "operator",
            "data_used_mb",
            "percent",
            "state",
        ),
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
