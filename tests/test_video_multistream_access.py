"""Per-leg video advertisement (`_stream_legs`) for the multi-stream cockpit
switcher: the primary leg is always served at the fixed ``main`` path and
secondary legs keep their ids, mirroring the Rust ``resolve_legs``."""

from types import SimpleNamespace

from ados.core.config.video import CameraLeg, VideoConfig
from ados.setup.service._access_urls import _stream_legs


def _cfg(cameras: list[CameraLeg]) -> SimpleNamespace:
    return SimpleNamespace(video=VideoConfig(cameras=cameras))


def test_backcompat_single_main_leg() -> None:
    legs = _stream_legs(_cfg([]), "drone.local", 8889, 8888)
    assert len(legs) == 1
    assert legs[0].id == "main"
    assert legs[0].whep_url == "http://drone.local:8889/main/whep"
    assert legs[0].hls_url == "http://drone.local:8888/main/index.m3u8"


def test_none_config_yields_single_main_leg() -> None:
    legs = _stream_legs(None, "drone.local", 8889, 8888)
    assert len(legs) == 1
    assert legs[0].id == "main"


def test_multi_leg_primary_is_main_secondaries_keep_ids() -> None:
    cams = [
        CameraLeg(id="eo-zoom", source="rtsp://pod/main", role="eo", codec="h265"),
        CameraLeg(id="eo-wide", source="rtsp://pod/sub", role="eo_wide", codec="h264"),
        CameraLeg(id="ir", source="rtsp://pod/ir", role="ir"),
    ]
    legs = _stream_legs(_cfg(cams), "drone.local", 8889, 8888)
    # No leg declared role "primary" → the first is the primary, served at main;
    # the sensor role is carried through for the GCS label.
    assert [leg.id for leg in legs] == ["main", "eo-wide", "ir"]
    assert [leg.role for leg in legs] == ["eo", "eo_wide", "ir"]
    assert legs[0].codec == "h265"
    assert legs[0].whep_url == "http://drone.local:8889/main/whep"
    assert legs[2].whep_url == "http://drone.local:8889/ir/whep"


def test_explicit_primary_role_is_served_at_main() -> None:
    cams = [
        CameraLeg(id="ir", source="rtsp://pod/ir", role="ir"),
        CameraLeg(id="eo", source="/dev/video0", role="primary"),
    ]
    legs = _stream_legs(_cfg(cams), "drone.local", 8889, 8888)
    # The declared-primary leg (index 1) is served at "main"; the first stays "ir".
    assert legs[1].id == "main"
    assert legs[0].id == "ir"
