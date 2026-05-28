"""Golden-output tests for the wizard step assembly.

These pin the canonical (id, state) tuple for each representative
operator-facing scenario. The state machine is pure (kwargs in, list
out) so a snapshot of the resulting tuple sequence catches any
unintended drift in step ordering, gating, or state derivation when
the assembly logic is moved or refactored.
"""

from __future__ import annotations

from ados.setup.models import (
    CloudChoiceStatus,
    HardwareCheckItem,
    HardwareCheckStatus,
    MavlinkAccess,
    NetworkStatus,
    ProfileSuggestion,
    RemoteAccessStatus,
    VideoAccess,
)
from ados.setup.state_machine import _setup_steps, build_setup_steps


def _ok_hc(profile: str) -> HardwareCheckStatus:
    """All-required-items-OK hardware check, profile-tagged."""
    return HardwareCheckStatus(
        profile=profile,
        items=[
            HardwareCheckItem(id="board", label="Board", required=True, state="ok"),
        ],
    )


def _shape(steps) -> list[tuple[str, str]]:
    return [(s.id, s.state) for s in steps]


def test_drone_profile_paired_with_cloud() -> None:
    """Fully wired drone — cloud paired, FC live, video running."""
    steps = _setup_steps(
        profile="drone",
        mavlink=MavlinkAccess(connected=True),
        video=VideoAccess(state="running"),
        network=NetworkStatus(local_ips=["10.0.0.5"]),
        remote=RemoteAccessStatus(status="running"),
        cloud_choice=CloudChoiceStatus(
            mode="cloud",
            backend_url="https://convex.altnautica.com",
            paired=True,
        ),
        profile_suggestion=ProfileSuggestion(detected="drone", confirmed=True),
        hardware_check=_ok_hc("drone"),
        mission_control_url="https://mc.local",
    )
    shape = _shape(steps)
    assert shape == [
        ("welcome", "complete"),
        ("profile", "complete"),
        ("hardware_check", "complete"),
        ("cloud_choice", "complete"),
        ("pair", "complete"),
        ("mavlink", "complete"),
        ("video", "complete"),
        ("remote_access", "complete"),
        ("finish", "complete"),
    ]


def test_drone_profile_local_only_hides_pair_step() -> None:
    """Local-only mode drops the pair step entirely."""
    steps = _setup_steps(
        profile="drone",
        mavlink=MavlinkAccess(connected=True),
        video=VideoAccess(),
        network=NetworkStatus(local_ips=["10.0.0.5"]),
        remote=RemoteAccessStatus(),
        cloud_choice=CloudChoiceStatus(mode="local"),
        profile_suggestion=ProfileSuggestion(detected="drone", confirmed=True),
        hardware_check=_ok_hc("drone"),
        mission_control_url="",
    )
    ids = [s.id for s in steps]
    assert "pair" not in ids
    cloud_step = next(s for s in steps if s.id == "cloud_choice")
    assert cloud_step.state == "complete"


def test_ground_station_profile_includes_ground_receiver_and_display() -> None:
    """Ground profile drops mavlink, includes ground_receiver + display."""
    steps = _setup_steps(
        profile="ground_station",
        mavlink=MavlinkAccess(),
        video=VideoAccess(state="running"),
        network=NetworkStatus(local_ips=["10.0.0.5"]),
        remote=RemoteAccessStatus(),
        cloud_choice=CloudChoiceStatus(
            mode="cloud",
            backend_url="https://convex.altnautica.com",
            paired=True,
        ),
        profile_suggestion=ProfileSuggestion(
            detected="ground_station",
            ground_role_hint="direct",
            confirmed=True,
        ),
        hardware_check=HardwareCheckStatus(
            profile="ground_station",
            items=[
                HardwareCheckItem(id="board", label="Board", required=True, state="ok"),
                HardwareCheckItem(id="display", label="Local display", state="ok", detail="Bound"),
            ],
        ),
        mission_control_url="",
    )
    shape = _shape(steps)
    assert shape == [
        ("welcome", "complete"),
        ("profile", "complete"),
        ("hardware_check", "complete"),
        ("cloud_choice", "complete"),
        ("pair", "complete"),
        ("video", "complete"),
        ("ground_receiver", "complete"),
        ("display", "complete"),
        ("remote_access", "optional"),
        ("finish", "complete"),
    ]


def test_ground_station_display_missing_marked_needs_action() -> None:
    """Ground profile with no LCD configured surfaces the display step."""
    steps = _setup_steps(
        profile="ground_station",
        mavlink=MavlinkAccess(),
        video=VideoAccess(),
        network=NetworkStatus(local_ips=["10.0.0.5"]),
        remote=RemoteAccessStatus(),
        cloud_choice=CloudChoiceStatus(mode="local"),
        profile_suggestion=ProfileSuggestion(
            detected="ground_station",
            ground_role_hint="direct",
            confirmed=True,
        ),
        hardware_check=HardwareCheckStatus(
            profile="ground_station",
            items=[
                HardwareCheckItem(id="board", label="Board", required=True, state="ok"),
            ],
        ),
        mission_control_url="",
    )
    display_step = next(s for s in steps if s.id == "display")
    assert display_step.state == "needs_action"


def test_drone_profile_camera_detected_video_pending() -> None:
    """Camera HAL probe ok but pipeline stopped — video_step needs_action."""
    steps = _setup_steps(
        profile="drone",
        mavlink=MavlinkAccess(connected=True),
        video=VideoAccess(),  # not running
        network=NetworkStatus(local_ips=["10.0.0.5"]),
        remote=RemoteAccessStatus(),
        cloud_choice=CloudChoiceStatus(mode="local"),
        profile_suggestion=ProfileSuggestion(detected="drone", confirmed=True),
        hardware_check=HardwareCheckStatus(
            profile="drone",
            items=[
                HardwareCheckItem(id="board", label="Board", required=True, state="ok"),
                HardwareCheckItem(id="camera", label="Camera", state="ok"),
            ],
        ),
        mission_control_url="",
    )
    video_step = next(s for s in steps if s.id == "video")
    assert video_step.state == "needs_action"
    assert "Click Start video" in video_step.detail


def test_fc_disconnected_mid_flow() -> None:
    """FC was connected, now reports disconnected — mavlink reverts to needs_action."""
    steps = _setup_steps(
        profile="drone",
        mavlink=MavlinkAccess(connected=False),
        video=VideoAccess(state="running"),
        network=NetworkStatus(local_ips=["10.0.0.5"]),
        remote=RemoteAccessStatus(),
        cloud_choice=CloudChoiceStatus(mode="cloud", paired=True, backend_url="https://x"),
        profile_suggestion=ProfileSuggestion(detected="drone", confirmed=True),
        hardware_check=_ok_hc("drone"),
        mission_control_url="",
    )
    mav_step = next(s for s in steps if s.id == "mavlink")
    assert mav_step.state == "needs_action"
    # Finish stays complete because video is running.
    finish_step = next(s for s in steps if s.id == "finish")
    assert finish_step.state == "complete"


def test_hardware_check_required_missing_blocks_step() -> None:
    """A required hardware-check item missing flips the hardware_check step."""
    steps = _setup_steps(
        profile="drone",
        mavlink=MavlinkAccess(),
        video=VideoAccess(),
        network=NetworkStatus(local_ips=["10.0.0.5"]),
        remote=RemoteAccessStatus(),
        cloud_choice=CloudChoiceStatus(),
        profile_suggestion=ProfileSuggestion(detected="drone", confirmed=True),
        hardware_check=HardwareCheckStatus(
            profile="drone",
            items=[
                HardwareCheckItem(id="board", label="Board", required=True, state="ok"),
                HardwareCheckItem(id="fc", label="FC", required=True, state="missing"),
            ],
        ),
        mission_control_url="",
    )
    hw_step = next(s for s in steps if s.id == "hardware_check")
    assert hw_step.state == "needs_action"


def test_build_setup_steps_alias_matches_underscore() -> None:
    """build_setup_steps is the public alias for _setup_steps."""
    assert build_setup_steps is _setup_steps
