"""Pydantic models for the universal setup surface."""

from __future__ import annotations

from typing import Literal

from pydantic import BaseModel, ConfigDict, Field

SetupStepState = Literal[
    "complete", "needs_action", "optional", "blocked", "not_applicable"
]


class SetupStep(BaseModel):
    id: str
    label: str
    state: SetupStepState
    detail: str = ""
    action_label: str = ""
    href: str = ""


class SetupAccessUrl(BaseModel):
    kind: Literal["setup", "api", "mission_control", "video", "mavlink", "cloud"]
    label: str
    url: str
    source: Literal["local", "hotspot", "usb", "mdns", "cloud", "configured"]
    primary: bool = False
    # Per-stream identity for a `kind="video"` entry when the node exposes more
    # than one video leg (a smart pod, a dual-camera rig). Empty on a single-leg
    # node, so existing consumers are unaffected. `id` is the mediamtx path /
    # WHEP id, `role` drives the GCS label (eo / eo_wide / ir / split), `codec`
    # lets the decoder pick the right path.
    id: str = ""
    role: str = ""
    codec: str = ""


class RemoteAccessStatus(BaseModel):
    provider: Literal["none", "cloudflare"] = "none"
    enabled: bool = False
    configured: bool = False
    status: Literal["disabled", "configured", "running", "stopped", "error"] = "disabled"
    public_urls: list[str] = Field(default_factory=list)
    error: str = ""


class VideoStreamAccess(BaseModel):
    """One video leg the node exposes: its stable id, role, codec, and the WHEP /
    HLS URLs a viewer connects to. On a single-leg node this is just the `main`
    stream; a multi-stream node (a smart pod) lists one per leg."""

    id: str = "main"
    role: str = ""
    codec: str = ""
    whep_url: str | None = None
    hls_url: str | None = None


class VideoAccess(BaseModel):
    state: str = "not_initialized"
    whep_url: str | None = None
    public_whep_url: str | None = None
    hls_url: str | None = None
    public_hls_url: str | None = None
    recording: bool = False
    # Per-leg streams when the node exposes more than one; the top-level
    # `whep_url` / `hls_url` stay the primary (`main`) leg for back-compat.
    streams: list[VideoStreamAccess] = Field(default_factory=list)


class MavlinkAccess(BaseModel):
    connected: bool = False
    port: str | None = None
    baud: int | None = None
    websocket_url: str | None = None
    public_websocket_url: str | None = None
    # Direct MAVLink TCP listener. Standard mavlink-router style endpoint.
    # The GCS can dial this without the WebSocket adapter when a desktop
    # GCS (Mission Planner, QGroundControl) is on the same LAN.
    tcp_url: str | None = None
    # Authenticated MAVLink WebSocket bridge served by the front on the
    # API port. Unlike ``websocket_url`` (a raw, unauthenticated port-8765
    # listener), this endpoint enforces the pairing-keyed ticket flow.
    # Populated only on the ground-station profile, where the gated bridge
    # exists; null/absent on drone and compute nodes. The GCS prefers it
    # when present. ``authenticated_websocket_path`` is the request path on
    # the agent's API surface; ``authenticated_websocket_url`` is the full
    # LAN-routable URL clients elsewhere on the LAN can dial.
    authenticated_websocket_path: str | None = None
    authenticated_websocket_url: str | None = None


class NetworkStatus(BaseModel):
    hostname: str = ""
    mdns_host: str = ""
    api_port: int = 8080
    hotspot_enabled: bool = False
    hotspot_ssid: str = ""
    local_ips: list[str] = Field(default_factory=list)
    uplink_kind: str | None = None
    wifi_ssid: str | None = None
    rssi_dbm: int | None = None
    ip_addresses: dict[str, str] = Field(default_factory=dict)


class ServiceState(BaseModel):
    """Light-weight service summary surfaced through the setup facade.

    The full per-service shape lives at /api/services. This model is the
    minimum the universal webapp and Mission Control's setup card need.
    """

    name: str
    state: str = "unknown"


class CloudChoiceStatus(BaseModel):
    """Cloud posture chosen during the onboarding wizard.

    Set by ``POST /api/v1/setup/cloud-choice`` and surfaced read-only on
    every ``SetupStatus`` response. Values mirror ``ServerConfig.mode``
    plus a small set of operator-facing diagnostics.
    """

    mode: Literal["cloud", "self_hosted", "local"] = "local"
    paired: bool = False
    pair_code_required: bool = False
    backend_url: str = ""  # display-only, never the API key
    backend_reachable: bool = False
    last_checked: str | None = None  # ISO 8601 IST


class ProfileSuggestion(BaseModel):
    """Result of the boot-time hardware fingerprint surfaced to the wizard.

    The wizard's profile step pre-selects ``detected`` and shows the per-
    signal map so the operator can sanity-check the auto-pick before
    confirming. ``confirmed`` flips true once the operator submits the
    profile step at least once for the active config value. ``source``
    marks which branch of the decision tail produced the profile so the
    dashboard can show "auto" vs "needs review" affordances cleanly.
    """

    detected: Literal["drone", "ground_station"] = "drone"
    source: Literal["detected", "tiebreaker", "override", "default"] = "detected"
    ground_role_hint: Literal["direct", "relay", "receiver"] = "direct"
    ground_score: int = 0
    air_score: int = 0
    mesh_capable: bool = False
    signals: dict[str, bool] = Field(default_factory=dict)
    confirmed: bool = False
    detected_at: str | None = None


class HardwareCheckItem(BaseModel):
    """One row in the hardware-check step's per-component readout."""

    id: str
    label: str
    required: bool = False
    state: Literal["ok", "missing", "warning", "checking", "unknown"] = "unknown"
    detail: str = ""
    fix_hint: str = ""


class HardwareCheckStatus(BaseModel):
    """Profile-aware hardware presence + readiness snapshot."""

    profile: str
    ground_role: str = ""
    items: list[HardwareCheckItem] = Field(default_factory=list)
    last_run: str = ""  # ISO 8601 IST


class SetupActionResult(BaseModel):
    ok: bool
    message: str
    data: dict[str, object] = Field(default_factory=dict)


class NetworkApplyRequest(BaseModel):
    """Network section of the batch-apply payload.

    Every field is optional so the caller only sends the slice they
    actually changed. ``wifi_password`` is write-only and is never
    echoed back through the response.
    """

    wifi_ssid: str | None = None
    wifi_password: str | None = None
    hotspot_enabled: bool | None = None


class WfbApplyRequest(BaseModel):
    """WFB section of the batch-apply payload.

    All fields optional. ``channel`` and ``tx_power_dbm`` mirror the
    existing dedicated routes; ``mcs_index`` and ``topology`` are
    persisted directly through this section because they have no
    other dedicated route. ``mcs_index`` and ``topology`` flips
    require a wfb-tx restart and are surfaced as reboot-required to
    the operator.
    """

    channel: int | None = None
    tx_power_dbm: int | None = None
    mcs_index: int | None = None
    topology: Literal["host_vbus", "powered_hub", "external_5v"] | None = None


class RegulatoryApplyRequest(BaseModel):
    """Operating-region section of the batch-apply payload.

    All fields optional so the caller sends only the slice they changed.
    ``mode`` is the posture: ``unrestricted`` (the default; radiate out
    of the box, operator responsible for local RF compliance) or
    ``region`` (pin an operating region and apply its channel/power
    limits). ``region`` is the ISO 3166-1 alpha-2 country code that
    applies when ``mode`` is ``region``; it is validated and uppercased
    by the setter. A region flip is reboot-required (the radio re-reads
    the posture on restart).
    """

    mode: Literal["unrestricted", "region"] | None = None
    region: str | None = None


class AdvancedApplyRequest(BaseModel):
    """Advanced section of the batch-apply payload.

    ``log_level`` is validated and written to ``config.logging.level``,
    then saved to disk (it applies on the next service restart).
    ``board_override`` is validated and written to the
    ``/etc/ados/board_override`` file the HAL detector reads; an empty
    value clears it. Both fields are optional so the caller sends only
    the slice they changed.
    """

    board_override: str | None = None
    log_level: str | None = None


class UiConfig(BaseModel):
    """UI section persisted on disk under ``ui:`` in config.yaml.

    Holds operator-facing presentation choices that the dashboard reads
    on every render tick. Theming is the only field today; future
    additions (page order, footer density, etc.) belong here. Strict
    schema (extra keys rejected) so a stale field on disk surfaces as
    a structured error instead of a silent ignore.
    """

    theme: Literal["dark", "light"] = "dark"

    model_config = ConfigDict(extra="forbid")


class UiApplyRequest(BaseModel):
    """UI section of the batch-apply payload.

    Every field is optional so the caller only sends the slice they
    actually changed. ``theme`` flips the dashboard palette live; the
    next render tick uses the new palette without a service restart.
    """

    theme: Literal["dark", "light"] | None = None


class DisplayOption(BaseModel):
    """One supported display the wizard offers in the picker.

    Mirrors a subset of the agent-side ``DisplayBinding`` shape from
    ``ados.hal.detect`` plus a synthetic ``id="none"`` option the wizard
    surfaces so the operator can explicitly skip without leaving the
    step in ``needs_action``.
    """

    id: str
    label: str
    controller: str | None = None
    touch_chip: str | None = None
    resolution: str | None = None


class DisplayOptionsResponse(BaseModel):
    """Read-only options payload consumed by the wizard's display step."""

    board_id: str
    current: dict[str, str] | None = None  # parsed /etc/ados/display.conf, or None
    supported: list[DisplayOption] = Field(default_factory=list)


class DisplayInstallRequest(BaseModel):
    """Operator's choice on the wizard's display step.

    ``display_id="none"`` is the explicit-skip path — the route writes a
    minimal ``display.conf`` with ``display_id=none`` and does not spawn
    the overlay installer.
    """

    display_id: str


DisplayJobStatus = Literal["queued", "running", "done", "failed"]


class DisplayJob(BaseModel):
    """Snapshot of a single ``install-display-overlay.sh`` job.

    The wizard polls the job endpoint at 1-2 Hz while ``status`` is
    ``queued`` or ``running`` and renders the trailing ``log_tail`` so
    the operator can watch the install progress in real time.
    """

    job_id: str
    status: DisplayJobStatus
    display_id: str
    started_at: str
    finished_at: str | None = None
    exit_code: int | None = None
    log_tail: list[str] = Field(default_factory=list)


class SetupStatus(BaseModel):
    version: str
    device_id: str
    device_name: str
    profile: str
    ground_role: str = ""
    setup_complete: bool
    setup_finalized: bool = False
    # Operator chose "Skip to Home" without finishing the wizard. The
    # webapp treats this and ``setup_finalized`` as equivalent gates
    # for routing to Home; the resume banner uses the pair to decide
    # whether to surface a "Resume setup" prompt.
    setup_skipped: bool = False
    setup_state: Literal["auto", "needs_review", "configured"] = "auto"
    profile_source: Literal["detected", "tiebreaker", "override", "default", "user"] = "detected"
    completion_percent: int
    next_action: str
    steps: list[SetupStep]
    access_urls: list[SetupAccessUrl]
    network: NetworkStatus
    mavlink: MavlinkAccess
    video: VideoAccess
    remote_access: RemoteAccessStatus
    services: list[ServiceState] = Field(default_factory=list)
    telemetry: dict[str, object] = Field(default_factory=dict)
    cloud_choice: CloudChoiceStatus = Field(default_factory=CloudChoiceStatus)
    profile_suggestion: ProfileSuggestion = Field(default_factory=ProfileSuggestion)
    hardware_check: HardwareCheckStatus | None = None
    skipped_steps: list[str] = Field(default_factory=list)
    # Pairing surface. ``pairing_code`` is the current 6-character code
    # the operator types into Mission Control to claim this device.
    # ``paired`` flips true once Mission Control completes the handshake.
    # Both fields live at the top level so the CLI and webapp can show
    # them without walking the steps array.
    pairing_code: str | None = None
    paired: bool = False
    # LAN-routable host the CLI and external clients should use to
    # reach this agent. Prefers the system hostname (``groundnode.local``)
    # over the device-id mDNS form (``ados-<id>.local``) when set, with
    # the first non-loopback IP as fallback. Empty when no LAN identity
    # could be derived. The agent's own webapp still self-references via
    # ``localhost``; this field is for clients elsewhere on the LAN.
    lan_host: str = ""
