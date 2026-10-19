"""The top-level ``cameras`` list on the config model."""

from ados.core.config import ADOSConfig
from ados.core.config.video import CameraLeg, VideoConfig


def test_the_simulated_rig_block_validates() -> None:
    # The keys the World Engine fleet VM writes for a simulated capture rig.
    cfg = ADOSConfig.model_validate(
        {
            "agent": {"profile": "visor"},
            "cameras": [
                {
                    "id": "cam_fl",
                    "role": "front_left",
                    "source": "rtsp://192.168.1.50:8554/we_v0_cam_fl",
                    "width": 1280,
                    "height": 800,
                    "fps": 20,
                    "format": "rgb24",
                    "capture": True,
                    "trigger": "hub",
                }
            ],
        }
    )
    cam = cfg.cameras[0]
    assert cam.capture is True
    assert cam.trigger == "hub"
    assert cam.trigger_channel is None
    assert cam.role == "front_left"


def test_capture_keys_default_off() -> None:
    leg = CameraLeg(id="belly", source="/dev/video2")
    assert leg.capture is False
    assert leg.format == "rgb24"
    assert leg.trigger == "none"
    assert leg.exposure_us is None
    assert leg.gain is None


def test_the_retired_lists_are_not_model_fields() -> None:
    assert "cameras" not in VideoConfig.model_fields
    assert "cameras" not in ADOSConfig.model_fields["vision"].annotation.model_fields  # type: ignore[union-attr]
    assert "cameras" in ADOSConfig.model_fields


def test_one_invalid_entry_is_skipped_not_the_whole_list() -> None:
    cfg = ADOSConfig.model_validate(
        {
            "cameras": [
                {"id": "eo", "source": "/dev/video0"},
                {"id": "bad", "capture": True, "exposure_us": 0},
                {"id": "ir", "source": "rtsp://pod/ir", "encoder": "turbo"},
                {"id": "belly", "source": "/dev/video4"},
            ]
        }
    )
    assert [c.id for c in cfg.cameras] == ["eo", "belly"]
