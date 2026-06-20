"""Tests for universal setup helpers."""

from __future__ import annotations

import pytest

from ados.setup.models import CloudChoiceStatus
from ados.setup.service import (
    _build_known_hosts,
    _cloud_choice_status,
    _mission_control_url,
    _safe_host_for,
    apply_cloud_choice,
    extract_cloudflare_token,
)


def test_extract_cloudflare_token_from_raw_token() -> None:
    assert extract_cloudflare_token("a" * 40) == "a" * 40


def test_extract_cloudflare_token_from_install_command() -> None:
    command = "sudo cloudflared service install abcdefghijklmnopqrstuvwxyz123456"
    assert extract_cloudflare_token(command) == "abcdefghijklmnopqrstuvwxyz123456"


def test_extract_cloudflare_token_rejects_missing_token() -> None:
    with pytest.raises(ValueError):
        extract_cloudflare_token("sudo cloudflared service install")


# --- MAVLink WS advertisement -----------------------------------------------


from ados.setup.models import MavlinkAccess  # noqa: E402


def test_mavlink_access_advertises_only_the_raw_proxy() -> None:
    """The MAVLink WS is authenticated on the raw ``websocket_url`` proxy via a
    ticket subprotocol; no separate gated endpoint is advertised, so the
    optional authenticated-endpoint fields default absent on every profile."""
    mav = MavlinkAccess(websocket_url="ws://host:8765/")
    assert mav.websocket_url == "ws://host:8765/"
    assert mav.authenticated_websocket_path is None
    assert mav.authenticated_websocket_url is None


# --- host-header validation ---------------------------------------------------


class _CfRemote:
    setup_url = ""
    api_url = ""


class _Cf:
    cloudflare = _CfRemote()


class _Cfg:
    remote_access = _Cf()


def test_safe_host_for_accepts_known_host() -> None:
    hosts = _build_known_hosts(
        local_ips=["10.0.0.5"], mdns_host="ados-test.local", config=_Cfg()
    )
    assert "10.0.0.5" in hosts
    assert "localhost" in hosts
    assert "192.168.4.1" in hosts  # hotspot
    assert "192.168.7.1" in hosts  # USB gadget
    assert "ados-test.local" in hosts
    assert _safe_host_for("10.0.0.5:8080", hosts) == "10.0.0.5:8080"
    assert _safe_host_for("ados-test.local:8080", hosts) == "ados-test.local:8080"


def test_safe_host_for_rejects_unknown_host() -> None:
    hosts = _build_known_hosts(local_ips=[], mdns_host="ados.local", config=_Cfg())
    assert _safe_host_for("attacker.example.com", hosts) == "localhost:8080"
    # Empty / missing falls back to localhost too.
    assert _safe_host_for("", hosts) == "localhost:8080"
    assert _safe_host_for(None, hosts) == "localhost:8080"


def test_safe_host_for_first_value_wins_in_chain() -> None:
    hosts = _build_known_hosts(local_ips=["10.0.0.5"], mdns_host="", config=_Cfg())
    assert _safe_host_for("10.0.0.5:8080, attacker.example.com", hosts) == "10.0.0.5:8080"


# --- Mission Control URL omission --------------------------------------------


class _Api:
    mission_control_url = ""


class _MCCfg:
    api = _Api()


def test_mission_control_url_returns_localhost_when_caller_is_localhost() -> None:
    assert _mission_control_url(host_name="localhost", config=_MCCfg()) == "http://localhost:4000"
    assert _mission_control_url(host_name="127.0.0.1", config=_MCCfg()) == "http://localhost:4000"


def test_mission_control_url_is_empty_for_remote_callers() -> None:
    # Phone on the hotspot / LAN: no useful localhost link.
    assert _mission_control_url(host_name="192.168.4.1", config=_MCCfg()) == ""
    assert _mission_control_url(host_name="10.0.0.5", config=_MCCfg()) == ""


def test_mission_control_url_honours_explicit_config() -> None:
    cfg = _MCCfg()
    cfg.api.mission_control_url = "https://command.altnautica.com"
    assert (
        _mission_control_url(host_name="10.0.0.5", config=cfg)
        == "https://command.altnautica.com"
    )


# --- cloud-choice -------------------------------------------------------------


class _Cloud:
    url = "https://convex-site.altnautica.com"
    mqtt_broker = "mqtt.altnautica.com"
    mqtt_port = 443


class _SelfHosted:
    url = ""
    mqtt_broker = ""
    mqtt_port = 8883
    api_key = ""


class _Server:
    mode = "local"
    cloud = _Cloud()
    self_hosted = _SelfHosted()
    mqtt_password = ""


class _Pairing:
    convex_url = ""


class _CloudChoiceConfig:
    server = _Server()
    pairing = _Pairing()


def test_cloud_choice_status_local_returns_minimal_record() -> None:
    cfg = _CloudChoiceConfig()
    cfg.server.mode = "local"
    cs = _cloud_choice_status(cfg)
    assert cs.mode == "local"
    assert cs.paired is False
    assert cs.pair_code_required is False
    assert cs.backend_url == ""


def test_cloud_choice_status_cloud_carries_backend_url() -> None:
    cfg = _CloudChoiceConfig()
    cfg.server.mode = "cloud"
    cs = _cloud_choice_status(cfg)
    assert cs.mode == "cloud"
    assert cs.pair_code_required is True
    assert cs.backend_url == "https://convex-site.altnautica.com"


def test_cloud_choice_status_self_hosted_reports_url() -> None:
    cfg = _CloudChoiceConfig()
    cfg.server.mode = "self_hosted"
    cfg.server.self_hosted.url = "https://convex.example.com"
    cs = _cloud_choice_status(cfg)
    assert cs.mode == "self_hosted"
    assert cs.backend_url == "https://convex.example.com"


class _RawRuntime:
    def save_config(self) -> None:
        pass


class _Runtime:
    def __init__(self) -> None:
        self.config = _CloudChoiceConfig()
        self.raw_runtime = _RawRuntime()


def test_apply_cloud_choice_local_clears_mqtt_password() -> None:
    runtime = _Runtime()
    runtime.config.server.mqtt_password = "leftover"
    result = apply_cloud_choice(runtime, mode="local")
    assert result.ok is True
    assert runtime.config.server.mode == "local"
    assert runtime.config.server.mqtt_password == ""


def test_apply_cloud_choice_self_hosted_persists_url_and_port() -> None:
    runtime = _Runtime()
    result = apply_cloud_choice(
        runtime,
        mode="self_hosted",
        self_hosted={
            "url": "https://convex.example.com",
            "mqtt_broker": "mqtt.example.com",
            "mqtt_port": 8884,
        },
    )
    assert result.ok is True
    assert runtime.config.server.mode == "self_hosted"
    assert runtime.config.server.self_hosted.url == "https://convex.example.com"
    assert runtime.config.server.self_hosted.mqtt_broker == "mqtt.example.com"
    assert runtime.config.server.self_hosted.mqtt_port == 8884
    # The backend URL is mirrored onto pairing.convex_url so the native cloud
    # beacon (which reads pairing.convex_url only) can beacon. A generic host
    # with no :3210 port and not the managed host is carried through unchanged
    # (the operator is expected to have entered the HTTP-actions/site origin).
    assert runtime.config.pairing.convex_url == "https://convex.example.com"


def test_apply_cloud_choice_self_hosted_requires_url() -> None:
    runtime = _Runtime()
    result = apply_cloud_choice(
        runtime,
        mode="self_hosted",
        self_hosted={"url": ""},
    )
    assert result.ok is False


def test_apply_cloud_choice_rejects_self_hosted_block_in_other_modes() -> None:
    runtime = _Runtime()
    result = apply_cloud_choice(
        runtime,
        mode="cloud",
        self_hosted={"url": "https://example.com"},
    )
    assert result.ok is False


def test_apply_cloud_choice_rejects_unknown_mode() -> None:
    runtime = _Runtime()
    result = apply_cloud_choice(runtime, mode="quantum")
    assert result.ok is False


def test_apply_cloud_choice_validates_port_range() -> None:
    runtime = _Runtime()
    result = apply_cloud_choice(
        runtime,
        mode="self_hosted",
        self_hosted={"url": "https://example.com", "mqtt_port": 99999},
    )
    assert result.ok is False


# --- AP defaults --------------------------------------------------------------


def test_hotspot_default_password_is_altnautica() -> None:
    """The agent's out-of-the-box AP passphrase is a known default so an
    operator can connect from a phone at the bench without reading a
    generated value off disk. Operators who need a unique passphrase set
    `network.hotspot.password` in /etc/ados/config.yaml.
    """
    from ados.core.config import HotspotConfig

    assert HotspotConfig().password == "altnautica"


# --- setup state (gate) ------------------------------------------------------


from pathlib import Path  # noqa: E402

from ados.setup import state as setup_state  # noqa: E402


def _redirect_state_path(monkeypatch, tmp_path: Path) -> Path:
    target = tmp_path / "state.json"
    monkeypatch.setattr(setup_state, "SETUP_STATE_PATH", target)
    monkeypatch.setattr(setup_state, "SETUP_STATE_DIR", tmp_path)
    return target


def test_setup_state_defaults_when_file_missing(monkeypatch, tmp_path) -> None:
    _redirect_state_path(monkeypatch, tmp_path)
    s = setup_state.read_state()
    assert s.setup_finalized is False
    assert s.skipped_steps == set()


def test_setup_state_mark_finalized_persists(monkeypatch, tmp_path) -> None:
    target = _redirect_state_path(monkeypatch, tmp_path)
    setup_state.mark_finalized()
    assert target.exists()
    s2 = setup_state.read_state()
    assert s2.setup_finalized is True


def test_setup_state_mark_and_clear_skip(monkeypatch, tmp_path) -> None:
    _redirect_state_path(monkeypatch, tmp_path)
    setup_state.mark_skipped("video")
    setup_state.mark_skipped("remote_access")
    s = setup_state.read_state()
    assert s.skipped_steps == {"video", "remote_access"}
    setup_state.clear_skipped("video")
    s2 = setup_state.read_state()
    assert s2.skipped_steps == {"remote_access"}


def test_setup_state_reset_clears_everything(monkeypatch, tmp_path) -> None:
    _redirect_state_path(monkeypatch, tmp_path)
    setup_state.mark_finalized()
    setup_state.mark_skipped("mavlink")
    setup_state.reset_state()
    s = setup_state.read_state()
    assert s.setup_finalized is False
    assert s.skipped_steps == set()


def test_setup_state_corrupt_file_returns_defaults(monkeypatch, tmp_path) -> None:
    target = _redirect_state_path(monkeypatch, tmp_path)
    target.write_text("not valid json {{{", encoding="utf-8")
    s = setup_state.read_state()
    assert s.setup_finalized is False
    assert s.skipped_steps == set()


def test_setup_state_record_ever_complete_grows_persisted_set(monkeypatch, tmp_path) -> None:
    """The percentage stays monotonic across transient state flips by
    promoting any currently-complete step id to a persisted set."""
    target = _redirect_state_path(monkeypatch, tmp_path)
    setup_state.record_ever_complete({"profile", "hardware_check"})
    s = setup_state.read_state()
    assert s.ever_completed_steps == {"profile", "hardware_check"}
    assert target.exists()


def test_setup_state_record_ever_complete_unions_with_prior(monkeypatch, tmp_path) -> None:
    _redirect_state_path(monkeypatch, tmp_path)
    setup_state.record_ever_complete({"profile"})
    setup_state.record_ever_complete({"hardware_check", "pair"})
    s = setup_state.read_state()
    assert s.ever_completed_steps == {"profile", "hardware_check", "pair"}


def test_setup_state_record_ever_complete_no_write_when_unchanged(
    monkeypatch, tmp_path,
) -> None:
    """Hot-path reads skip the write when the persisted set already
    contains every id passed in."""
    target = _redirect_state_path(monkeypatch, tmp_path)
    setup_state.record_ever_complete({"profile", "hardware_check"})
    mtime_before = target.stat().st_mtime_ns
    # No new ids: no write.
    setup_state.record_ever_complete({"profile"})
    mtime_after = target.stat().st_mtime_ns
    assert mtime_before == mtime_after


def test_setup_state_reset_clears_ever_completed(monkeypatch, tmp_path) -> None:
    _redirect_state_path(monkeypatch, tmp_path)
    setup_state.record_ever_complete({"profile", "hardware_check", "pair"})
    setup_state.reset_state()
    s = setup_state.read_state()
    assert s.ever_completed_steps == set()


def test_setup_state_legacy_file_without_ever_completed_key(
    monkeypatch, tmp_path,
) -> None:
    """A state file written by an older agent that doesn't know about
    ever_completed_steps must read back cleanly with an empty set."""
    target = _redirect_state_path(monkeypatch, tmp_path)
    target.write_text(
        '{"setup_finalized": true, "skipped_steps": ["remote_access"]}',
        encoding="utf-8",
    )
    s = setup_state.read_state()
    assert s.setup_finalized is True
    assert s.skipped_steps == {"remote_access"}
    assert s.ever_completed_steps == set()


# --- profile choice ----------------------------------------------------------


from ados.setup.profile import apply_profile, build_profile_suggestion  # noqa: E402


class _AgentCfg:
    def __init__(self, profile: str = "auto") -> None:
        self.profile = profile
        self.device_id = "test"
        self.name = "test"


class _GroundStationCfg:
    def __init__(self, role: str = "direct") -> None:
        self.role = role
        self.share_uplink = False


class _ProfileConfig:
    def __init__(self, profile: str = "auto", role: str = "direct") -> None:
        self.agent = _AgentCfg(profile)
        self.ground_station = _GroundStationCfg(role)


class _ProfileRuntime:
    def __init__(self, profile: str = "auto", role: str = "direct") -> None:
        self.config = _ProfileConfig(profile, role)
        self.raw_runtime = _RawRuntime()


def test_apply_profile_accepts_drone() -> None:
    runtime = _ProfileRuntime(profile="auto")
    result = apply_profile(runtime, profile="drone")
    assert result.ok is True
    assert runtime.config.agent.profile == "drone"
    assert result.data.get("ground_role") == ""


def test_apply_profile_accepts_ground_station_with_role() -> None:
    runtime = _ProfileRuntime(profile="auto")
    result = apply_profile(runtime, profile="ground_station", ground_role="relay")
    assert result.ok is True
    assert runtime.config.agent.profile == "ground_station"
    assert runtime.config.ground_station.role == "relay"


def test_apply_profile_defaults_ground_role_to_direct() -> None:
    runtime = _ProfileRuntime(profile="auto")
    result = apply_profile(runtime, profile="ground_station")
    assert result.ok is True
    assert runtime.config.ground_station.role == "direct"


def test_apply_profile_rejects_unknown_profile() -> None:
    runtime = _ProfileRuntime()
    result = apply_profile(runtime, profile="quadcopter")  # type: ignore[arg-type]
    assert result.ok is False


def test_apply_profile_rejects_unknown_role() -> None:
    runtime = _ProfileRuntime()
    result = apply_profile(
        runtime, profile="ground_station", ground_role="omega"
    )
    assert result.ok is False


def test_build_profile_suggestion_marks_explicit_pick_confirmed(monkeypatch) -> None:
    cfg = _ProfileConfig(profile="drone", role="direct")
    monkeypatch.setattr(
        "ados.bootstrap.profile_detect.detect_profile",
        lambda config_override=None: {
            "profile": "drone",
            "ground_score": 0,
            "air_score": 5,
            "signals": {"mavlink_serial": True},
            "mesh_capable": False,
            "detected_at": "2026-05-04T20:00:00+05:30",
        },
    )
    sug = build_profile_suggestion(cfg)
    assert sug.confirmed is True
    assert sug.detected == "drone"


def test_build_profile_suggestion_unconfirmed_when_profile_auto(monkeypatch) -> None:
    cfg = _ProfileConfig(profile="auto")
    monkeypatch.setattr(
        "ados.bootstrap.profile_detect.detect_profile",
        lambda config_override=None: {
            "profile": "ground_station",
            "ground_score": 5,
            "air_score": 0,
            "signals": {"oled_i2c": True, "buttons_gpio": True},
            "mesh_capable": False,
            "detected_at": "2026-05-04T20:00:00+05:30",
        },
    )
    sug = build_profile_suggestion(cfg)
    assert sug.confirmed is False
    assert sug.detected == "ground_station"
    assert sug.signals.get("oled_i2c") is True


# --- hardware check ----------------------------------------------------------


from ados.setup.hardware_check import (  # noqa: E402
    derive_step_state,
    run_hardware_check,
)
from ados.setup.models import HardwareCheckItem, HardwareCheckStatus  # noqa: E402


def test_derive_step_state_complete_when_all_required_ok() -> None:
    snap = HardwareCheckStatus(
        profile="drone",
        items=[
            HardwareCheckItem(id="board", label="Board", required=True, state="ok"),
            HardwareCheckItem(id="fc", label="FC", required=True, state="ok"),
            HardwareCheckItem(id="cam", label="Camera", required=True, state="ok"),
            HardwareCheckItem(id="gps", label="GPS", required=False, state="warning"),
        ],
    )
    state, _detail = derive_step_state(snap)
    assert state == "complete"


def test_derive_step_state_needs_action_when_required_missing() -> None:
    snap = HardwareCheckStatus(
        profile="drone",
        items=[
            HardwareCheckItem(id="board", label="Board", required=True, state="ok"),
            HardwareCheckItem(id="fc", label="FC", required=True, state="missing"),
        ],
    )
    state, _detail = derive_step_state(snap)
    assert state == "needs_action"


def test_run_hardware_check_drone_emits_required_items(monkeypatch) -> None:
    # Stub out the heavy probes so the test does not hit real hardware.
    monkeypatch.setattr(
        "ados.hal.detect.detect_board",
        lambda force=False: type(
            "B",
            (),
            {
                "name": "test",
                "model": "test",
                "tier": 3,
                "ram_mb": 4096,
                "cpu_cores": 4,
            },
        )(),
    )
    monkeypatch.setattr("ados.hal.camera.discover_cameras", lambda: [])
    monkeypatch.setattr(
        "ados.bootstrap.profile_detect.probe_mavlink_serial",
        lambda: (0, 0, False),
    )
    monkeypatch.setattr(
        "ados.bootstrap.profile_detect.probe_gps_serial", lambda: (0, 0, False)
    )
    monkeypatch.setattr("ados.hal.usb.discover_usb_devices", lambda: [])
    monkeypatch.setattr("ados.hal.modem.detect_modem", lambda: None)

    snap = run_hardware_check(None, profile="drone")
    ids = [item.id for item in snap.items]
    assert "board" in ids
    assert "fc" in ids
    assert "camera" in ids
    # FC is required and missing -> step needs action
    state, _ = derive_step_state(snap)
    assert state == "needs_action"


def test_run_hardware_check_ground_relay_requires_mesh_dongle(monkeypatch) -> None:
    monkeypatch.setattr(
        "ados.hal.detect.detect_board",
        lambda force=False: type(
            "B",
            (),
            {
                "name": "pi4b",
                "model": "Raspberry Pi 4 Model B",
                "tier": 3,
                "ram_mb": 4096,
                "cpu_cores": 4,
            },
        )(),
    )
    monkeypatch.setattr("ados.hal.usb.discover_usb_devices", lambda: [])
    monkeypatch.setattr(
        "ados.bootstrap.profile_detect.probe_mesh_capable", lambda: False
    )
    monkeypatch.setattr(
        "ados.bootstrap.profile_detect.probe_i2c_oled", lambda bus=1: (0, 0, False)
    )
    monkeypatch.setattr(
        "ados.bootstrap.profile_detect.probe_gpio_buttons",
        lambda pins=None: (0, 0, False),
    )
    monkeypatch.setattr(
        "ados.bootstrap.profile_detect.probe_uplink_type", lambda: (0, 0, False)
    )
    monkeypatch.setattr("ados.hal.modem.detect_modem", lambda: None)

    snap = run_hardware_check(None, profile="ground_station", ground_role="relay")
    item_ids = {item.id for item in snap.items}
    assert "radio_wfb" in item_ids
    assert "mesh_dongle" in item_ids
    mesh = next(item for item in snap.items if item.id == "mesh_dongle")
    assert mesh.required is True
    assert mesh.state == "missing"


# --- step assembler with profile + hardware_check ---------------------------


from ados.setup.models import (  # noqa: E402
    HardwareCheckItem as _HCItem,
)
from ados.setup.models import (
    HardwareCheckStatus as _HCStatus,
)
from ados.setup.models import (
    MavlinkAccess as _Mav,
)
from ados.setup.models import (
    NetworkStatus as _Net,
)
from ados.setup.models import (
    ProfileSuggestion as _Sug,
)
from ados.setup.models import (
    RemoteAccessStatus as _Rem,
)
from ados.setup.models import (
    VideoAccess as _Vid,
)
from ados.setup.service import _setup_steps  # noqa: E402


def _all_complete_hc(profile: str) -> _HCStatus:
    return _HCStatus(
        profile=profile,
        items=[
            _HCItem(id="x", label="x", required=True, state="ok"),
        ],
    )


def test_setup_steps_emits_profile_step_after_welcome() -> None:
    steps = _setup_steps(
        profile="drone",
        mavlink=_Mav(),
        video=_Vid(),
        network=_Net(local_ips=["10.0.0.5"]),
        remote=_Rem(),
        cloud_choice=CloudChoiceStatus(),
        profile_suggestion=_Sug(detected="drone", confirmed=True),
        hardware_check=_all_complete_hc("drone"),
        mission_control_url="",
    )
    ids = [s.id for s in steps]
    assert ids[:2] == ["welcome", "profile"]
    assert "region" in ids
    assert ids.index("region") == ids.index("profile") + 1
    assert "hardware_check" in ids
    assert ids.index("hardware_check") == ids.index("region") + 1


def test_setup_steps_drone_includes_mavlink_and_skips_ground_receiver() -> None:
    steps = _setup_steps(
        profile="drone",
        mavlink=_Mav(),
        video=_Vid(),
        network=_Net(local_ips=["10.0.0.5"]),
        remote=_Rem(),
        cloud_choice=CloudChoiceStatus(),
        profile_suggestion=_Sug(detected="drone", confirmed=True),
        hardware_check=_all_complete_hc("drone"),
        mission_control_url="",
    )
    ids = {s.id for s in steps}
    assert "mavlink" in ids
    assert "ground_receiver" not in ids


def test_setup_steps_ground_station_includes_ground_receiver_and_skips_mavlink() -> None:
    steps = _setup_steps(
        profile="ground_station",
        mavlink=_Mav(),
        video=_Vid(),
        network=_Net(local_ips=["10.0.0.5"]),
        remote=_Rem(),
        cloud_choice=CloudChoiceStatus(),
        profile_suggestion=_Sug(
            detected="ground_station",
            ground_role_hint="direct",
            confirmed=True,
        ),
        hardware_check=_all_complete_hc("ground_station"),
        mission_control_url="",
    )
    ids = {s.id for s in steps}
    assert "ground_receiver" in ids
    assert "mavlink" not in ids


def test_setup_steps_profile_step_needs_action_when_unconfirmed() -> None:
    steps = _setup_steps(
        profile="auto",
        mavlink=_Mav(),
        video=_Vid(),
        network=_Net(local_ips=["10.0.0.5"]),
        remote=_Rem(),
        cloud_choice=CloudChoiceStatus(),
        profile_suggestion=_Sug(detected="drone", confirmed=False),
        hardware_check=_all_complete_hc("drone"),
        mission_control_url="",
    )
    profile_step = next(s for s in steps if s.id == "profile")
    assert profile_step.state == "needs_action"


def test_setup_steps_hardware_check_state_follows_required_items() -> None:
    incomplete = _HCStatus(
        profile="drone",
        items=[
            _HCItem(id="board", label="Board", required=True, state="ok"),
            _HCItem(id="fc", label="FC", required=True, state="missing"),
        ],
    )
    steps = _setup_steps(
        profile="drone",
        mavlink=_Mav(),
        video=_Vid(),
        network=_Net(local_ips=["10.0.0.5"]),
        remote=_Rem(),
        cloud_choice=CloudChoiceStatus(),
        profile_suggestion=_Sug(detected="drone", confirmed=True),
        hardware_check=incomplete,
        mission_control_url="",
    )
    hw_step = next(s for s in steps if s.id == "hardware_check")
    assert hw_step.state == "needs_action"
