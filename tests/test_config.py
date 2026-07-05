"""Tests for config loading, validation, and defaults."""

from __future__ import annotations

import tempfile

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


def test_load_config_no_file():
    """Loading from a non-existent path should return defaults."""
    cfg = load_config("/tmp/nonexistent-ados-config-12345.yaml")
    assert cfg.agent.name == "my-drone"


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


def test_ws_proxy_enforce_auth_defaults_off_and_round_trips():
    """The WS-proxy auth-enforcement flag defaults off and survives a round trip
    through the config model (so it is not stripped and can be set the sanctioned
    way rather than hand-editing the on-disk config)."""
    assert ADOSConfig().mavlink.ws_proxy_enforce_auth is False
    with tempfile.NamedTemporaryFile("w", suffix=".yaml", delete=False) as f:
        yaml.safe_dump({"mavlink": {"ws_proxy_enforce_auth": True}}, f)
        path = f.name
    cfg = load_config(path)
    assert cfg.mavlink.ws_proxy_enforce_auth is True


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


# ─── Per-board VideoConfig.use_gst_air_pipeline default ───────────────────────


def test_use_gst_air_pipeline_defaults_true_on_rockchip(monkeypatch):
    """On Rockchip boards the in-process GStreamer pipeline is preferred
    so encoding can offload to ``mpph264enc`` on the VPU. The
    AirPipeline's own encoder chooser falls back to ``x264enc`` if
    ``mpph264enc`` is missing at runtime, so this default is safe even
    on a Rockchip rig without the rockchip-mpp gstreamer plugin
    installed."""
    from ados.core.config.video import VideoConfig

    class _FakeBoard:
        soc = "RK3582"

    monkeypatch.setattr(
        "ados.hal.detect.detect_board", lambda *a, **kw: _FakeBoard()
    )
    cfg = VideoConfig()
    assert cfg.use_gst_air_pipeline is True


def test_use_gst_air_pipeline_defaults_false_on_pi(monkeypatch):
    """On non-Rockchip boards (Pi 4B BCM2711, Cubie A7Z Allwinner, etc.)
    the per-board default is False so we stay on the bench-validated
    legacy bash pipeline. Operators can still opt in via config.yaml."""
    from ados.core.config.video import VideoConfig

    class _FakeBoard:
        soc = "BCM2711"

    monkeypatch.setattr(
        "ados.hal.detect.detect_board", lambda *a, **kw: _FakeBoard()
    )
    cfg = VideoConfig()
    assert cfg.use_gst_air_pipeline is False


def test_use_gst_air_pipeline_respects_explicit_config(monkeypatch):
    """An explicit ``use_gst_air_pipeline: false`` in config.yaml wins
    over the per-board True default. The operator override path must
    never be silently flipped by a future board-detection refactor."""
    from ados.core.config.video import VideoConfig

    class _FakeBoard:
        soc = "RK3588"  # Would otherwise default to True

    monkeypatch.setattr(
        "ados.hal.detect.detect_board", lambda *a, **kw: _FakeBoard()
    )
    cfg = VideoConfig(use_gst_air_pipeline=False)
    assert cfg.use_gst_air_pipeline is False


def test_use_gst_air_pipeline_falls_back_to_false_on_detect_failure(
    monkeypatch,
):
    """Board fingerprint probe failures (HAL not importable in a unit
    test fixture, /proc unreadable in a container) must never crash
    config loading. The default factory swallows the exception and
    returns the conservative False."""
    from ados.core.config.video import VideoConfig

    def _boom(*a, **kw):
        raise RuntimeError("simulated /proc parse failure")

    monkeypatch.setattr(
        "ados.hal.detect.detect_board", _boom
    )
    cfg = VideoConfig()
    assert cfg.use_gst_air_pipeline is False
