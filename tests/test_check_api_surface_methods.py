"""The client-route check judges the HTTP method a call site states.

A route served only for another method answers 405, the same silent break a
missing path is, so a path-only match passed exactly the breaks the check
exists to catch (a PUT to a POST-only route). These drive the method
inference and the (method, path) decision directly; the full run needs every
client checkout.
"""

from __future__ import annotations

import importlib.util
from pathlib import Path

_SCRIPT = Path(__file__).resolve().parent.parent / "scripts" / "check-api-surface.py"
_spec = importlib.util.spec_from_file_location("check_api_surface", _SCRIPT)
assert _spec is not None and _spec.loader is not None
chk = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(chk)

TABLE = [
    ("POST", "/api/v1/network/client/join".split("/")),
    ("WS", "/api/v1/ground-station/ws/uplink".split("/")),
    ("DELETE", "/api/plugins/{plugin_id}/perms/{permission_id}".split("/")),
    ("WS", "/api/plugins/jobs/{job_id}".split("/")),
    ("GET", "/api/plugins/{plugin_id}/config".split("/")),
]


def test_a_method_the_route_does_not_serve_is_refused() -> None:
    method = chk.stated_method("  await fetch(`{}", '`, {\n  method: "PUT",\n});')
    assert method == "PUT"
    assert not chk.method_served("/api/v1/network/client/join", method, TABLE)


def test_the_served_method_passes() -> None:
    assert chk.method_served("/api/v1/network/client/join", "POST", TABLE)
    assert chk.method_served("/api/plugins/{}/perms/{}", "DELETE", TABLE)


def test_verb_arguments_and_verb_methods_state_the_method() -> None:
    assert chk.stated_method('request("PUT", `{}', "`)") == "PUT"
    assert chk.stated_method('    r = client.delete(f"{base}', '")') == "DELETE"


def test_a_websocket_url_is_checked_as_ws() -> None:
    assert chk.stated_method("new WebSocket(`ws://{}:8080", "`)") == "WS"
    assert chk.method_served("/api/v1/ground-station/ws/uplink", "WS", TABLE)


def test_a_plain_get_to_a_websocket_only_route_is_refused() -> None:
    # A poll of the install-progress socket with fetch() is a GET the route
    # never answers. The interpolated id also matches `{plugin_id}/config`,
    # but the literal `jobs` segment makes the socket route the one it names.
    assert chk.websocket_only("/api/plugins/jobs/{}", TABLE)
    assert not chk.method_served("/api/plugins/jobs/{}", "GET", TABLE)
    assert chk.method_served("/api/plugins/jobs/{}", "WS", TABLE)
    assert chk.method_served("/api/plugins/{}/config", "GET", TABLE)


def test_the_next_calls_options_are_not_borrowed() -> None:
    # A GET with no options, followed by a POST to another path: the POST's
    # options belong to the second literal.
    after = '`);\nawait fetch(`/api/other`, { method: "POST" });'
    assert chk.stated_method("  await fetch(`", after) is None


def test_an_agent_mcp_call_is_not_mistaken_for_the_apps_own_route() -> None:
    # The app serves /api/mcp/activity/stream itself; the agent serves other
    # /api/mcp/* routes, which must still be checked.
    own = [["", "api", "mcp", "activity", "stream"], ["", "api", "lan-pair", "{param}"]]
    assert chk.owns("/api/mcp/activity/stream", own)
    assert chk.owns("/api/lan-pair/{}", own)
    assert not chk.owns("/api/mcp/tokens", own)
