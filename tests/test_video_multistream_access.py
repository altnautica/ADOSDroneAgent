"""Per-leg video advertisement (`_stream_legs`) for the multi-stream cockpit
switcher: the primary leg is always served at the fixed ``main`` path and
secondary legs keep their ids, mirroring the Rust ``resolve_legs``."""

from types import SimpleNamespace

from ados.core.config.video import CameraLeg, VideoConfig
from ados.setup.service._access_urls import _stream_legs, _video_viewer_url


def _cfg(cameras: list[CameraLeg]) -> SimpleNamespace:
    return SimpleNamespace(video=VideoConfig(cameras=cameras))


def test_backcompat_single_main_leg() -> None:
    legs = _stream_legs(_cfg([]))
    assert len(legs) == 1
    assert legs[0].id == "main"
    # Relative to the agent's front: the media server binds loopback and is
    # reached only through the front's credentialed proxy.
    assert legs[0].whep_url == "/whep?camera=main"
    assert legs[0].hls_url == "/hls/main/index.m3u8"


def test_none_config_yields_single_main_leg() -> None:
    legs = _stream_legs(None)
    assert len(legs) == 1
    assert legs[0].id == "main"


def test_multi_leg_primary_is_main_secondaries_keep_ids() -> None:
    cams = [
        CameraLeg(id="eo-zoom", source="rtsp://pod/main", role="eo", codec="h265"),
        CameraLeg(id="eo-wide", source="rtsp://pod/sub", role="eo_wide", codec="h264"),
        CameraLeg(id="ir", source="rtsp://pod/ir", role="ir"),
    ]
    legs = _stream_legs(_cfg(cams))
    # No leg declared role "primary" → the first is the primary, served at main;
    # the sensor role is carried through for the GCS label.
    assert [leg.id for leg in legs] == ["main", "eo-wide", "ir"]
    assert [leg.role for leg in legs] == ["eo", "eo_wide", "ir"]
    assert legs[0].codec == "h265"
    assert legs[0].whep_url == "/whep?camera=main"
    assert legs[2].whep_url == "/whep?camera=ir"
    assert legs[2].hls_url == "/hls/ir/index.m3u8"


def test_explicit_primary_role_is_served_at_main() -> None:
    cams = [
        CameraLeg(id="ir", source="rtsp://pod/ir", role="ir"),
        CameraLeg(id="eo", source="/dev/video0", role="primary"),
    ]
    legs = _stream_legs(_cfg(cams))
    # The declared-primary leg (index 1) is served at "main"; the first stays "ir".
    assert legs[1].id == "main"
    assert legs[0].id == "ir"


def test_the_video_link_is_the_cockpit_on_the_front_never_a_media_port() -> None:
    # A relative WHEP URL lives on the front; the operator link is the cockpit.
    assert _video_viewer_url("http://drone.local:8080", "/whep") == (
        "http://drone.local:8080/cockpit/"
    )
    # A tunnel URL fronting the media server keeps its player page.
    assert _video_viewer_url("http://x", "https://tunnel.example.com/main/whep") == (
        "https://tunnel.example.com/main/"
    )
    assert _video_viewer_url("http://x", None) == ""
