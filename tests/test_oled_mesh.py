"""Tests for mesh-mode OLED screens and overlay dispatch.

Covers:
- Menu visibility filter on role + mesh_capable state
- Overlay enter/exit lifecycle including nested transitions
- First-boot unset mode guard and auto-focus on `Set role`
- Each mesh screen renders without exceptions against a representative
  state dict, using a PIL ImageDraw as the draw target
"""

from __future__ import annotations

import asyncio
from typing import Any
from unittest.mock import MagicMock

import pytest
from PIL import Image, ImageDraw


def _draw() -> ImageDraw.ImageDraw:
    img = Image.new("1", (128, 64))
    return ImageDraw.Draw(img)


# ---------------------------------------------------------------------------
# Menu visibility filter
# ---------------------------------------------------------------------------


def test_menu_filter_collapses_mesh_when_not_capable():
    """When mesh is not capable, the Mesh entry stays visible but its
    submenu collapses to a single "Mesh unavailable" hint item."""
    from ados.services.ui.oled_service import MENU_TREE, _filter_visible

    state = {"role": {"current": "direct", "mesh_capable": False}}
    labels = [n.get("label") for n in _filter_visible(MENU_TREE, state)]
    assert "Mesh" in labels
    mesh_node = next(n for n in MENU_TREE if n.get("label") == "Mesh")
    child_labels = [
        n.get("label") for n in _filter_visible(mesh_node.get("children") or [], state)
    ]
    assert child_labels == ["Mesh unavailable"]


def test_menu_filter_shows_mesh_when_capable():
    from ados.services.ui.oled_service import MENU_TREE, _filter_visible

    state = {"role": {"current": "direct", "mesh_capable": True}}
    labels = [n.get("label") for n in _filter_visible(MENU_TREE, state)]
    assert "Mesh" in labels


def test_mesh_submenu_filters_by_role():
    from ados.services.ui.oled_service import MENU_TREE, _filter_visible

    mesh_node = next(n for n in MENU_TREE if n.get("label") == "Mesh")
    children = mesh_node.get("children") or []

    receiver_state = {"role": {"current": "receiver", "mesh_capable": True}, "mesh": {}}
    labels = [n.get("label") for n in _filter_visible(children, receiver_state)]
    assert "Accept relay" in labels
    assert "Join mesh" not in labels

    relay_state = {"role": {"current": "relay", "mesh_capable": True}, "mesh": {"up": False}}
    labels = [n.get("label") for n in _filter_visible(children, relay_state)]
    assert "Join mesh" in labels
    assert "Accept relay" not in labels


def test_leave_mesh_only_visible_when_mesh_up():
    from ados.services.ui.oled_service import MENU_TREE, _filter_visible

    mesh_node = next(n for n in MENU_TREE if n.get("label") == "Mesh")
    children = mesh_node.get("children") or []

    up_state = {"role": {"current": "relay", "mesh_capable": True}, "mesh": {"up": True}}
    labels = [n.get("label") for n in _filter_visible(children, up_state)]
    assert "Leave mesh" in labels

    down_state = {"role": {"current": "relay", "mesh_capable": True}, "mesh": {"up": False}}
    labels = [n.get("label") for n in _filter_visible(children, down_state)]
    assert "Leave mesh" not in labels


# ---------------------------------------------------------------------------
# Screen render smoke
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    "module_name,state",
    [
        ("unset_boot", {"role": {"current": None, "mesh_capable": True}}),
        ("role_picker", {"role": {"current": "direct"}, "_overlay_state": {"role_idx": 0}}),
        (
            "accept_window",
            {
                "pairing": {
                    "window": {"closes_at_ms": 0},
                    "pending": [{"device_id": "relay-alpha"}],
                },
                "_overlay_state": {"cursor": 0},
            },
        ),
        ("join_scan", {"mesh": {"scan": {"found_host": "rx-alpha", "link_quality": 85}}}),
        ("join_request_inflight", {"_overlay_state": {"started_ms": 0}}),
        (
            "joined_status",
            {
                "mesh": {"mesh_id": "ados-test", "up": True, "peer_count": 2},
                "_overlay_state": {"receiver_host": "rx-alpha"},
            },
        ),
        ("hub_unreachable", {"mesh": {"hub_lost_since_ms": 0}}),
        (
            "neighbors",
            {"mesh": {"neighbors": [{"mac": "aa:bb", "tq": 200}]}, "_overlay_state": {"cursor": 0}},
        ),
        ("leave_confirm", {}),
        ("error_states", {"_overlay_state": {"code": "E_JOIN_TIMEOUT", "message": "no invite"}}),
    ],
)
def test_mesh_screen_renders(module_name: str, state: dict):
    from ados.services.ui.oled_service import OVERLAY_SCREENS

    mod = OVERLAY_SCREENS[module_name]
    mod.render(_draw(), 128, 64, state)


# ---------------------------------------------------------------------------
# OledService overlay dispatch (no hardware)
# ---------------------------------------------------------------------------


def _make_service():
    from ados.services.ui.events import ButtonEventBus
    from ados.services.ui.oled_service import OledService

    svc = OledService(bus=ButtonEventBus(), api_host="127.0.0.1", api_port=9999)
    svc._state = {
        "role": {"current": "receiver", "mesh_capable": True, "configured": "receiver"},
        "mesh": {"up": False, "peer_count": 0},
    }

    class FakeResp:
        status_code = 200
        content = b"{}"

        def json(self) -> dict:
            return {}

    class FakeHttp:
        async def get(self, *a: Any, **kw: Any) -> FakeResp:
            return FakeResp()

        async def post(self, *a: Any, **kw: Any) -> FakeResp:
            return FakeResp()

        async def put(self, *a: Any, **kw: Any) -> FakeResp:
            return FakeResp()

        async def aclose(self) -> None:
            return None

    svc._http = FakeHttp()
    return svc


def test_overlay_enter_sets_state_and_mode():
    svc = _make_service()
    svc._enter_overlay("role_picker")
    assert svc._mode == "overlay"
    assert svc._overlay_id == "role_picker"
    # initial_state() returns the cursor index of the current role
    assert "role_idx" in svc._overlay_state


def test_overlay_exit_clears_state_and_mode():
    svc = _make_service()
    svc._enter_overlay("neighbors")
    svc._exit_overlay()
    assert svc._mode == "status"
    assert svc._overlay_id is None
    assert svc._overlay_module is None
    assert svc._overlay_state == {}


def test_nested_overlay_enter_fires_previous_on_exit():
    """Transitioning overlay -> overlay must call on_exit on the first module."""
    on_exit_calls: list[str] = []
    from ados.services.ui.screens.mesh import accept_window

    async def fake_on_exit(service: Any) -> None:
        on_exit_calls.append("accept_window")

    # on_enter/on_exit are scheduled via asyncio.create_task, which
    # requires a running loop. Drive the whole sequence inside one.
    async def _run() -> None:
        svc = _make_service()
        original_on_enter = getattr(accept_window, "on_enter", None)
        original_on_exit = getattr(accept_window, "on_exit", None)
        # Stub on_enter so the real version does not try to drive the
        # pairing poll (it would race with the test).

        async def noop_on_enter(service: Any) -> None:
            return None

        accept_window.on_enter = noop_on_enter  # type: ignore[attr-defined]
        accept_window.on_exit = fake_on_exit  # type: ignore[attr-defined]
        try:
            svc._enter_overlay("accept_window")
            svc._enter_overlay("error_states", initial_state={"code": "E_PAIR_WINDOW_EXPIRED"})
            # Give the scheduled on_exit task a chance to run.
            await asyncio.sleep(0.05)
        finally:
            if original_on_enter is None:
                try:
                    delattr(accept_window, "on_enter")
                except AttributeError:
                    pass
            else:
                accept_window.on_enter = original_on_enter  # type: ignore[attr-defined]
            if original_on_exit is None:
                try:
                    delattr(accept_window, "on_exit")
                except AttributeError:
                    pass
            else:
                accept_window.on_exit = original_on_exit  # type: ignore[attr-defined]

    asyncio.run(_run())
    assert on_exit_calls == ["accept_window"], (
        "nested overlay entry must fire previous overlay's on_exit"
    )


def test_overlay_b4_exits_when_unmapped():
    """B4 pressed in an overlay without a B4 handler exits back to status."""
    svc = _make_service()
    svc._enter_overlay("joined_status")  # only maps B4 -> exit
    asyncio.run(svc._handle_overlay_press(19))  # B4
    assert svc._mode == "status"


def test_first_boot_unset_detected():
    svc = _make_service()
    svc._state = {"role": {"current": "unset", "mesh_capable": True}}
    assert svc._first_boot_unset() is True

    svc._state = {"role": {"current": "direct", "mesh_capable": True}}
    assert svc._first_boot_unset() is False

    # Not mesh-capable: never first-boot-unset
    svc._state = {"role": {"current": None, "mesh_capable": False}}
    assert svc._first_boot_unset() is False


def test_role_badge_respects_mesh_capable():
    """Badge should not render when mesh_capable is False."""
    svc = _make_service()
    svc._state = {"role": {"current": "direct", "mesh_capable": False}, "mesh": {}}
    draw = MagicMock()
    svc._render_role_badge(draw)
    draw.text.assert_not_called()


def test_role_badge_receiver_with_mesh_id():
    svc = _make_service()
    svc._state = {
        "role": {"current": "receiver", "mesh_capable": True},
        "mesh": {"mesh_id": "ados-xyz-123"},
    }
    draw = MagicMock()
    svc._render_role_badge(draw)
    draw.text.assert_called_once()
    args, _kwargs = draw.text.call_args
    (x, _y), label = args[0], args[1]
    # Clamp keeps badge to left of WIDTH, right of 94 boundary
    assert x >= 94
    assert x <= 128
    # Label starts with Rx and clamps mesh_id to 3 chars
    assert label.startswith("Rx")
    assert len(label) <= 5
