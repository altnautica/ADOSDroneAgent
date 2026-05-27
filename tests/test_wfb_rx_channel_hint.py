"""Tests for the WFB locked-channel runtime hint and cold-start homing.

The rendezvous home channel (``video.wfb.channel`` in config.yaml) is the
operator's immutable meeting point: both drone and ground start there and
return there on link loss. The receiver must never overwrite it. A
successful sweep lock is recorded only as a runtime HINT on tmpfs so a
restart has a fast first guess, with the home channel always the fallback.

Coverage:
  1. ``_persist_locked_channel`` writes the runtime hint file (not config).
  2. ``_persist_locked_channel`` does NOT modify ``video.wfb.channel`` in
     config.yaml, even when a config file is present.
  3. ``_read_locked_channel_hint`` round-trips and tolerates missing /
     corrupt content.
  4. On a cold start with no link, ``_channel`` equals the configured home
     channel, not any persisted lock.
"""

from __future__ import annotations

from unittest.mock import patch

from ados.core.config import WfbConfig


def _make_rx_manager(cfg: WfbConfig):
    from ados.services.ground_station.wfb_rx import WfbRxManager

    with patch("ados.services.wfb.manager._apply_link_preset"):
        return WfbRxManager(cfg)


def test_persist_locked_channel_writes_hint_file(tmp_path):
    cfg = WfbConfig(interface="wlan1", channel=149, band="u-nii-3")
    mgr = _make_rx_manager(cfg)

    hint = tmp_path / "wfb-locked-channel"
    with patch(
        "ados.core.paths.WFB_LOCKED_CHANNEL_HINT", hint
    ):
        mgr._persist_locked_channel(153)

    assert hint.is_file()
    assert hint.read_text(encoding="utf-8").strip() == "153"


def test_persist_locked_channel_never_touches_config_channel(tmp_path):
    """The rendezvous home in config.yaml must remain the operator's value."""
    import yaml

    cfg = WfbConfig(interface="wlan1", channel=149, band="u-nii-3")
    mgr = _make_rx_manager(cfg)

    config_yaml = tmp_path / "config.yaml"
    config_yaml.write_text(
        yaml.safe_dump({"video": {"wfb": {"channel": 149}}}),
        encoding="utf-8",
    )
    hint = tmp_path / "wfb-locked-channel"

    with patch("ados.core.paths.WFB_LOCKED_CHANNEL_HINT", hint), patch(
        "ados.core.paths.CONFIG_YAML", config_yaml
    ):
        mgr._persist_locked_channel(161)

    # config.yaml's home channel is untouched.
    reloaded = yaml.safe_load(config_yaml.read_text(encoding="utf-8"))
    assert reloaded["video"]["wfb"]["channel"] == 149
    # The locked channel lives only in the runtime hint.
    assert hint.read_text(encoding="utf-8").strip() == "161"


def test_read_locked_channel_hint_round_trip(tmp_path):
    from ados.services.ground_station.wfb_rx import WfbRxManager

    hint = tmp_path / "wfb-locked-channel"
    hint.write_text("157\n", encoding="utf-8")
    with patch("ados.core.paths.WFB_LOCKED_CHANNEL_HINT", hint):
        assert WfbRxManager._read_locked_channel_hint() == 157


def test_read_locked_channel_hint_missing_returns_none(tmp_path):
    from ados.services.ground_station.wfb_rx import WfbRxManager

    hint = tmp_path / "does-not-exist"
    with patch("ados.core.paths.WFB_LOCKED_CHANNEL_HINT", hint):
        assert WfbRxManager._read_locked_channel_hint() is None


def test_read_locked_channel_hint_corrupt_returns_none(tmp_path):
    from ados.services.ground_station.wfb_rx import WfbRxManager

    hint = tmp_path / "wfb-locked-channel"
    for junk in ("", "   ", "not-a-number", "-3", "abc\n"):
        hint.write_text(junk, encoding="utf-8")
        with patch("ados.core.paths.WFB_LOCKED_CHANNEL_HINT", hint):
            assert WfbRxManager._read_locked_channel_hint() is None


def test_cold_start_channel_is_configured_home():
    """A freshly constructed receiver homes on config.video.wfb.channel."""
    cfg = WfbConfig(interface="wlan1", channel=149, band="u-nii-3")
    mgr = _make_rx_manager(cfg)
    assert mgr._channel == 149

    cfg2 = WfbConfig(interface="wlan1", channel=44, band="u-nii-1")
    mgr2 = _make_rx_manager(cfg2)
    assert mgr2._channel == 44
