"""Tests for config loading, validation, and defaults."""

from __future__ import annotations

import tempfile

import pytest
import yaml

from ados.core.config import ADOSConfig, load_config


def test_default_config():
    """ADOSConfig with no args should have sensible defaults."""
    cfg = ADOSConfig()
    assert cfg.agent.name == "my-drone"
    assert cfg.mavlink.baud_rate == 57600
    assert cfg.mavlink.system_id == 1
    assert cfg.mavlink.component_id == 191
    assert cfg.logging.level == "info"
    assert cfg.swarm.enabled is False


def test_device_id_auto_generated():
    """Empty device_id should be auto-filled."""
    cfg = ADOSConfig()
    assert cfg.agent.device_id != ""
    assert len(cfg.agent.device_id) == 8


def test_load_config_from_yaml():
    """Config loaded from YAML should override defaults."""
    data = {
        "agent": {"name": "test-drone", "tier": "tier3"},
        "mavlink": {"baud_rate": 921600},
    }
    with tempfile.NamedTemporaryFile(mode="w", suffix=".yaml", delete=False) as f:
        yaml.dump(data, f)
        f.flush()
        cfg = load_config(f.name)

    assert cfg.agent.name == "test-drone"
    assert cfg.agent.tier == "tier3"
    assert cfg.mavlink.baud_rate == 921600
    # Defaults should still be intact
    assert cfg.logging.level == "info"


def test_load_config_no_file(monkeypatch, tmp_path):
    """With no config file anywhere in the search order, pure defaults load.

    The search order is explicit path -> ``CONFIG_YAML`` -> ``./config.yaml`` ->
    defaults, so passing only a bogus explicit path is not enough: on a machine
    that has an agent installed (``/etc/ados/config.yaml``, or ``~/.ados/
    config.yaml`` on a rootless macOS install) the second candidate wins and the
    test read that host's real node name. Both remaining candidates are pointed
    at empty ground here so the assertion is about the defaults, not the host.
    """
    monkeypatch.setattr(
        "ados.core.config.CONFIG_YAML", tmp_path / "absent" / "config.yaml"
    )
    monkeypatch.chdir(tmp_path)
    cfg = load_config(str(tmp_path / "nonexistent-ados-config.yaml"))
    assert cfg.agent.name == "my-drone"
    assert cfg.mavlink.baud_rate == 57600
    assert cfg.logging.level == "info"


def test_config_extra_ignored():
    """Unknown keys in YAML should be silently ignored."""
    data = {
        "agent": {"name": "test"},
        "unknown_section": {"foo": "bar"},
    }
    with tempfile.NamedTemporaryFile(mode="w", suffix=".yaml", delete=False) as f:
        yaml.dump(data, f)
        f.flush()
        cfg = load_config(f.name)
    assert cfg.agent.name == "test"


def test_regulatory_defaults_unrestricted():
    """A fresh config defaults the operating-region posture to unrestricted."""
    cfg = ADOSConfig()
    assert cfg.network.regulatory.mode == "unrestricted"
    assert cfg.network.regulatory.region is None
    assert cfg.network.regulatory.ack_operator is None
    assert cfg.network.regulatory.ack_at is None


def test_regulatory_no_block_reads_unrestricted():
    """A config file with no network.regulatory block reads as unrestricted."""
    data = {"agent": {"name": "x"}, "network": {"hotspot": {"enabled": True}}}
    with tempfile.NamedTemporaryFile(mode="w", suffix=".yaml", delete=False) as f:
        yaml.dump(data, f)
        f.flush()
        cfg = load_config(f.name)
    assert cfg.network.regulatory.mode == "unrestricted"
    assert cfg.network.regulatory.region is None


def test_crsf_lane_defaults_off_and_unpinned():
    """A fresh config has no CRSF pin and the lane opted out."""
    cfg = ADOSConfig()
    assert cfg.radio.crsf.enabled is False
    assert cfg.radio.crsf.device is None
    assert cfg.radio.crsf.band == "dual"
    assert cfg.radio.crsf.packet_rate_hz == 150
    assert cfg.radio.crsf.tx_power_dbm is None
    assert cfg.radio.crsf.mode == "crsf_rc"
    assert cfg.radio.crsf.channel_source == "hid"
    assert cfg.radio.crsf.mavlink_transport == "serial"
    assert cfg.radio.crsf.mavlink_command_enabled is False
    assert cfg.radio.crsf.relay_role == "none"


def test_crsf_pin_round_trips_through_a_full_save():
    """The radio.crsf block survives a model_dump() full-file rewrite.

    Every config save rewrites the whole YAML from ``model_dump()``; a section
    missing from the model would be silently dropped on the next write, erasing
    the operator's pin. The section must therefore be modelled, not merely
    tolerated by ``extra: ignore``.
    """
    data = {
        "radio": {
            "crsf": {
                "enabled": True,
                "device": "/dev/ttyUSB0",
                "band": "900",
                "packet_rate_hz": 250,
                "tx_power_dbm": 20,
                "mode": "mavlink",
                "channel_source": "hybrid",
                "mavlink_transport": "backpack_wifi",
                "mavlink_command_enabled": True,
                "relay_role": "repeater",
            }
        }
    }
    with tempfile.NamedTemporaryFile(mode="w", suffix=".yaml", delete=False) as f:
        yaml.dump(data, f)
        f.flush()
        cfg = load_config(f.name)
    assert cfg.radio.crsf.enabled is True
    assert cfg.radio.crsf.device == "/dev/ttyUSB0"
    assert cfg.radio.crsf.band == "900"
    assert cfg.radio.crsf.packet_rate_hz == 250
    assert cfg.radio.crsf.tx_power_dbm == 20
    assert cfg.radio.crsf.mode == "mavlink"
    assert cfg.radio.crsf.channel_source == "hybrid"
    assert cfg.radio.crsf.mavlink_transport == "backpack_wifi"
    assert cfg.radio.crsf.mavlink_command_enabled is True
    assert cfg.radio.crsf.relay_role == "repeater"
    dumped = cfg.model_dump()
    assert dumped["radio"]["crsf"] == data["radio"]["crsf"]


def test_crsf_unset_nullables_dump_as_null():
    """The unset nullable fields (device pin, TX power) dump as ``None`` — the
    on-disk YAML carries explicit nulls, which every native reader of the block
    tolerates. A ``region``-style Literal typo is rejected by validation rather
    than silently defaulted (the native readers degrade loudly instead)."""
    import pytest
    from pydantic import ValidationError

    from ados.core.config.radio import CrsfConfig

    dumped = ADOSConfig().model_dump()["radio"]["crsf"]
    assert dumped["device"] is None
    assert dumped["tx_power_dbm"] is None
    with pytest.raises(ValidationError):
        CrsfConfig(band="5ghz")
    with pytest.raises(ValidationError):
        CrsfConfig(channel_source="bogus")


def test_regulatory_region_round_trips():
    """A pinned operating region round-trips through the YAML loader unchanged."""
    data = {
        "network": {
            "regulatory": {
                "mode": "region",
                "region": "IN",
                "ack_operator": "op1",
                "ack_at": "2026-06-03T10:00:00+05:30",
            }
        }
    }
    with tempfile.NamedTemporaryFile(mode="w", suffix=".yaml", delete=False) as f:
        yaml.dump(data, f)
        f.flush()
        cfg = load_config(f.name)
    reg = cfg.network.regulatory
    assert reg.mode == "region"
    assert reg.region == "IN"
    assert reg.ack_operator == "op1"
    assert reg.ack_at == "2026-06-03T10:00:00+05:30"
    # model_dump is byte-stable through a YAML round-trip.
    from ados.core.config import RegulatoryConfig

    dumped = reg.model_dump()
    reloaded = RegulatoryConfig(**yaml.safe_load(yaml.safe_dump(dumped)))
    assert reloaded == reg


def test_load_config_tolerates_unquoted_timestamp():
    """An unquoted ISO-8601 timestamp (as the native config writers emit for
    video.wfb.paired_at) must load as a string, not a datetime that would fail
    the str-typed field and crash the API at startup."""
    raw = (
        "profile: drone\n"
        "video:\n"
        "  mode: auto\n"
        "  wfb:\n"
        "    paired_at: 2026-05-30T10:18:35+00:00\n"
        "    auto_pair_enabled: false\n"
    )
    with tempfile.NamedTemporaryFile(mode="w", suffix=".yaml", delete=False) as f:
        f.write(raw)
        f.flush()
        cfg = load_config(f.name)
    assert isinstance(cfg.video.wfb.paired_at, str)
    assert cfg.video.wfb.paired_at == "2026-05-30T10:18:35+00:00"


def test_mavlink_endpoints_default():
    """Default endpoints should include one WebSocket on 8765."""
    cfg = ADOSConfig()
    assert len(cfg.mavlink.endpoints) >= 1
    assert cfg.mavlink.endpoints[0].type == "websocket"
    assert cfg.mavlink.endpoints[0].port == 8765


def test_ws_proxy_enforce_auth_defaults_on_and_round_trips():
    """The WS-proxy auth-enforcement flag defaults on, matching the router that
    reads it, and survives a round trip through the config model (so it is not
    stripped and an operator who needs it off can say so the sanctioned way
    rather than hand-editing the on-disk config)."""
    assert ADOSConfig().mavlink.ws_proxy_enforce_auth is True
    with tempfile.NamedTemporaryFile("w", suffix=".yaml", delete=False) as f:
        yaml.safe_dump({"mavlink": {"ws_proxy_enforce_auth": False}}, f)
        path = f.name
    cfg = load_config(path)
    assert cfg.mavlink.ws_proxy_enforce_auth is False


def test_security_defaults():
    """Security defaults should be reasonable."""
    cfg = ADOSConfig()
    assert cfg.security.tls.enabled is True
    assert cfg.security.api.cors_enabled is True
    assert len(cfg.security.api.cors_origins) >= 1
    assert "*" not in cfg.security.api.cors_origins
    assert "http://localhost:4000" in cfg.security.api.cors_origins


def test_cors_origins_additive_merge():
    """Custom cors_origins config keeps the default Mission Control origins.

    A deployment yaml that sets `cors_origins:` to a custom list
    must not accidentally drop the dev / local Mission Control
    origin. The effective allowlist is always defaults+configured+extras.
    """
    from ados.core.config import ApiSecurityConfig

    cfg = ApiSecurityConfig(cors_origins=["https://team.example.com"])
    effective = cfg.effective_cors_origins
    assert "http://localhost:4000" in effective
    assert "https://team.example.com" in effective
    # No duplicates.
    assert len(effective) == len(set(effective))


def test_cors_origins_extra_merges():
    """`cors_origins_extra` augments on top of defaults."""
    from ados.core.config import ApiSecurityConfig

    cfg = ApiSecurityConfig(cors_origins_extra=["https://team.example.com"])
    effective = cfg.effective_cors_origins
    assert "http://localhost:4000" in effective
    assert "https://team.example.com" in effective


def test_cors_origins_env_override_replaces(monkeypatch):
    """`ADOS_CORS_ORIGINS_OVERRIDE` env var fully replaces the allowlist."""
    from ados.core.config import ApiSecurityConfig

    monkeypatch.setenv(
        "ADOS_CORS_ORIGINS_OVERRIDE",
        "https://only-this.example.com, https://and-this.example.com ",
    )
    cfg = ApiSecurityConfig()
    effective = cfg.effective_cors_origins
    assert effective == [
        "https://only-this.example.com",
        "https://and-this.example.com",
    ]
    assert "http://localhost:4000" not in effective




def test_camera_leg_management_field_defaults_match_rust():
    """A CameraLeg with only id/source declared defaults every management field
    to the same value the Rust `CameraLeg` does (name/orientation/owner/fov/mount/
    calibration/match absent, purpose empty, enabled True), so a leg declared
    before the roster fields existed reads identically on both halves."""
    from ados.core.config.video import CameraLeg

    leg = CameraLeg(id="belly", source="/dev/video2")
    assert leg.name is None
    assert leg.orientation is None
    assert leg.purpose == []
    assert leg.enabled is True
    assert leg.owner is None
    assert leg.fov_deg is None
    assert leg.mount_pitch_deg is None
    assert leg.calibration is None
    assert leg.camera_match is None


def test_camera_leg_management_fields_round_trip():
    """The full management field set parses, including the ``match`` wire key
    aliased to ``camera_match`` (``match`` is a Python keyword)."""
    from ados.core.config.video import CameraLeg

    leg = CameraLeg.model_validate(
        {
            "id": "belly",
            "source": "/dev/video2",
            "role": "primary",
            "codec": "h265",
            "name": "Belly cam",
            "orientation": "down",
            "purpose": ["detect", "precision-landing"],
            "enabled": False,
            "owner": "operator",
            "fov_deg": 82.5,
            "mount_pitch_deg": -45.0,
            "calibration": "belly-v1",
            "match": {"usb": "046d:0825:ABC123"},
        }
    )
    assert leg.name == "Belly cam"
    assert leg.orientation == "down"
    assert leg.purpose == ["detect", "precision-landing"]
    assert leg.enabled is False
    assert leg.owner == "operator"
    assert leg.fov_deg == 82.5
    assert leg.mount_pitch_deg == -45.0
    assert leg.calibration == "belly-v1"
    assert leg.camera_match is not None
    assert leg.camera_match.usb == "046d:0825:ABC123"
    # A CSI fingerprint parses the sensor + port.
    csi = CameraLeg.model_validate(
        {"id": "nadir", "source": "/dev/video0", "match": {"csi_sensor": "imx219", "csi_port": 1}}
    )
    assert csi.camera_match is not None
    assert csi.camera_match.csi_sensor == "imx219"
    assert csi.camera_match.csi_port == 1
    assert csi.camera_match.usb is None


def test_profile_accepts_workstation_and_compute():
    """The config profile enum accepts every profile a fleet node runs as. A
    workstation or compute node sets its profile explicitly, so its config must
    validate across the drone / ground-station / workstation / compute set."""
    from ados.core.config.agent import AgentConfig

    for profile in ("auto", "drone", "ground_station", "workstation", "compute"):
        assert AgentConfig(profile=profile).profile == profile

    with pytest.raises(ValueError):
        AgentConfig(profile="not-a-profile")


def test_packaged_defaults_carry_no_orphan_top_level_keys():
    """Every top-level key in the packaged defaults must be a config field.

    The model ignores unknown keys (``extra: ignore``) and the config
    persist path is a full model-dump rewrite, so a defaults block with
    no matching model field is silently dropped at load and can never
    round-trip through a write — dead config. Guard the 1:1
    correspondence so the next orphan block cannot land.
    """
    from importlib.resources import files

    text = files("ados.core").joinpath("defaults.yaml").read_text(encoding="utf-8")
    data = yaml.safe_load(text)
    assert isinstance(data, dict) and data, "packaged defaults must parse to a mapping"

    orphans = sorted(set(data) - set(ADOSConfig.model_fields))
    assert not orphans, (
        f"defaults.yaml top-level keys with no ADOSConfig field: {orphans}; "
        "declare a model field or delete the block"
    )


def test_swarm_defaults_carry_the_behaviour_blocks():
    """The swarm block ships the flocking / separation / task subtrees.

    The GCS Swarm settings page binds directly to these dot-paths, so a
    missing subtree renders "not set" rows the operator cannot write.
    """
    swarm = ADOSConfig().swarm
    assert swarm.mode == "hold"
    assert swarm.default_formation == "line"
    # Gains are integer percentages of the float weight the runtime uses.
    assert (swarm.flock.cohesion, swarm.flock.alignment) == (40, 60)
    assert swarm.flock.separation_gain == 150
    assert (swarm.flock.radius_m, swarm.flock.neighbors) == (30, 7)
    assert (swarm.separation.radius_m, swarm.separation.hard_m) == (8, 4)
    # Agent-written assignment mirrors stay unset until a runtime writes them.
    assert swarm.tasks.enabled is False
    assert swarm.tasks.assigned_task_id is None
    assert swarm.tasks.bundle_position is None


def test_swarm_rejects_a_formation_outside_the_builtin_set():
    """An unknown formation name produced no formation at all, silently."""
    with pytest.raises(ValueError):
        ADOSConfig(swarm={"default_formation": "diamond"})


def test_swarm_separation_hard_floor_must_sit_inside_the_repulsion_radius():
    """hard_m >= radius_m means the hard floor fires before repulsion ever
    engages — the safety layer inverted. Reject it at load."""
    with pytest.raises(ValueError):
        ADOSConfig(swarm={"separation": {"radius_m": 4, "hard_m": 8}})
    # The boundary is exclusive: equal radii leave zero repulsion band.
    with pytest.raises(ValueError):
        ADOSConfig(swarm={"separation": {"radius_m": 6, "hard_m": 6}})
    assert ADOSConfig(swarm={"separation": {"radius_m": 6, "hard_m": 5}})


def test_swarm_has_no_lora_subtree():
    """LoRa had no driver and no consumer anywhere in the agent; the dead
    subtree is gone rather than left for a UI to surface."""
    assert "lora" not in ADOSConfig.model_fields["swarm"].annotation.model_fields


def test_camera_hero_defaults_are_unchanged_and_thumbnail_is_the_small_profile():
    """The fleet shares one 20 MHz channel, so exactly one drone streams full
    video. The top-level camera fields ARE the hero profile and must not have
    moved; the new ``thumbnail`` block is what every other drone runs."""
    cam = ADOSConfig().video.camera
    assert (cam.width, cam.height, cam.fps, cam.bitrate_kbps) == (1280, 720, 30, 4000)
    thumb = cam.thumbnail
    assert (thumb.width, thumb.height, thumb.fps, thumb.bitrate_kbps) == (320, 180, 1, 50)


def test_camera_thumbnail_is_overridable_per_field():
    """A partial ``thumbnail:`` block keeps the remaining defaults, so an
    operator tuning one knob never silently resets the other three."""
    cfg = ADOSConfig(video={"camera": {"thumbnail": {"fps": 2}}})
    thumb = cfg.video.camera.thumbnail
    assert thumb.fps == 2
    assert (thumb.width, thumb.height, thumb.bitrate_kbps) == (320, 180, 50)


# ---------------------------------------------------------------------------
# ws_proxy_enforce_auth: dropping a recorded value so the shipped default wins
# ---------------------------------------------------------------------------


def _ws_migrate(tmp_path, mavlink_block):
    """Run the migration over a config file and return (in_memory, on_disk)."""
    import yaml as _yaml

    from ados.core.config import _migrators

    _migrators._WS_ENFORCE_DEFAULT_MIGRATED = False
    cfg = tmp_path / "config.yaml"
    cfg.write_text(_yaml.safe_dump({"mavlink": mavlink_block, "video": {}}))
    raw = _yaml.safe_load(cfg.read_text())
    out = _migrators._migrate_ws_proxy_enforce_default(raw, cfg)
    return out, _yaml.safe_load(cfg.read_text())


def test_a_recorded_enforce_false_is_dropped_from_memory_and_disk(tmp_path):
    # The whole point: every node written while the old posture shipped has
    # `false` in its own file, and an explicit value beats a default -- so
    # changing the default in code reached none of them. Proven on hardware,
    # where the proxy logged `enforce_auth=false admitted=true` while running a
    # build whose default was true.
    mem, disk = _ws_migrate(tmp_path, {"system_id": 1, "ws_proxy_enforce_auth": False})
    assert "ws_proxy_enforce_auth" not in mem["mavlink"]
    assert "ws_proxy_enforce_auth" not in disk["mavlink"]


def test_an_operator_who_chose_enforcement_is_left_alone(tmp_path):
    # `true` says something the default cannot say. Removing it would be
    # rewriting a deliberate choice.
    mem, disk = _ws_migrate(tmp_path, {"ws_proxy_enforce_auth": True})
    assert mem["mavlink"]["ws_proxy_enforce_auth"] is True
    assert disk["mavlink"]["ws_proxy_enforce_auth"] is True


def test_the_value_is_removed_rather_than_rewritten_to_true(tmp_path):
    # Writing `true` would freeze today's answer into the file and reproduce
    # this exact problem the next time the shipped posture moves.
    _, disk = _ws_migrate(tmp_path, {"ws_proxy_enforce_auth": False})
    assert "ws_proxy_enforce_auth" not in disk["mavlink"]


def test_neighbouring_mavlink_settings_survive(tmp_path):
    # The migration rewrites the file, so anything an operator tuned beside it
    # has to come back untouched.
    _, disk = _ws_migrate(
        tmp_path,
        {"system_id": 42, "baud_rate": 921600, "ws_proxy_enforce_auth": False},
    )
    assert disk["mavlink"]["system_id"] == 42
    assert disk["mavlink"]["baud_rate"] == 921600


def test_a_config_without_the_key_is_untouched(tmp_path):
    mem, disk = _ws_migrate(tmp_path, {"system_id": 1})
    assert mem["mavlink"] == {"system_id": 1}
    assert disk["mavlink"] == {"system_id": 1}


def test_the_declared_default_matches_the_router_that_reads_it():
    # The router's own default for this key is on. Persisting the config dumps
    # every field including defaults, so a Python default of `false` is not a
    # difference of opinion -- it is written into the node's file as an
    # explicit value, and an explicit value beats the router's default.
    from ados.core.config.mavlink import MavlinkConfig

    assert MavlinkConfig().ws_proxy_enforce_auth is True


def test_the_migration_is_not_undone_by_the_next_config_write(tmp_path):
    # The removal migration and the model default have to agree, because the
    # save path re-serializes the whole model. Dropping the recorded value and
    # then writing the old default straight back would leave the node exactly
    # where it started, with the migration reporting success.
    import yaml as _yaml

    from ados.core.config.mavlink import MavlinkConfig

    mem, _ = _ws_migrate(tmp_path, {"system_id": 1, "ws_proxy_enforce_auth": False})
    reloaded = MavlinkConfig(**mem["mavlink"])
    rewritten = _yaml.safe_load(_yaml.safe_dump(reloaded.model_dump()))
    assert rewritten["ws_proxy_enforce_auth"] is True
