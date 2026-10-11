"""The client-route check reads the Android client's root-relative routes.

The Kotlin client names every route without a leading slash
(`get("api/status")`), builds ground-station paths from a file-local base
constant (`"$BASE/wfb"`), keeps socket paths as constants, and interpolates
bare `$name` values. A scan that only knows slash-led literals finds none of
them and so checks nothing. These drive the rewrite and the verb inference on
fixture text.
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
    ("GET", "/api/status".split("/")),
    ("GET", "/api/v1/ground-station/wfb".split("/")),
    ("PUT", "/api/v1/ground-station/wfb".split("/")),
    ("WS", "/api/v1/ground-station/ws/uplink".split("/")),
    ("GET", "/api/plugins/{plugin_id}/gcs/{*asset_path}".split("/")),
    ("POST", "/api/plugins/{plugin_id}/enable".split("/")),
    ("DELETE", "/api/v1/ground-station/wfb/pair/{device_id}".split("/")),
]

FIXTURE = '''\
class Api(private val client: AgentClient) {
    suspend fun status() = get("api/status", JsonObject.serializer())
    suspend fun wfb() = client.get("$BASE/wfb", WfbConfig.serializer())
    suspend fun putWfb(channel: Int) =
        client.send(
            "PUT",
            "$BASE/wfb",
            WfbUpdate.serializer(),
        )
    suspend fun asset(id: String, encoded: String) = client.bytes("api/plugins/${seg(id)}/gcs/$encoded")
    suspend fun enable(id: String) = post("api/plugins/${seg(id)}/enable")
    suspend fun release(id: String) = client.requestJson("DELETE", "api/v1/ground-station/wfb/pair/${enc(id, "UTF-8")}")
    suspend fun gone() = client.requestJson("GET", "api/v1/ground-station/radar")
    private companion object {
        const val BASE = "api/v1/ground-station"
    }
}
enum class GsStream(val path: String) {
    Uplink("api/v1/ground-station/ws/uplink"),
}
val label = "$title api/status"
'''


def _sites() -> list[tuple[str | None, str]]:
    """(verb, normalised path) for every literal the scan reads in FIXTURE."""
    lines = chk.kotlin_paths(FIXTURE.splitlines())
    out = []
    for lineno, raw in enumerate(lines, 1):
        line = chk.INTERPOLATION.sub("{}", raw)
        for m in chk.LITERAL.finditer(line):
            before = line[: m.start()]
            verb = chk.stated_method(before, line[m.end() :]) or chk.kotlin_method(before, lines, lineno)
            out.append((verb, chk.normalise(m.group(0))))
    return out


def test_root_relative_routes_are_read_with_their_verb() -> None:
    sites = _sites()
    assert ("GET", "/api/status") in sites
    assert ("GET", "/api/v1/ground-station/wfb") in sites
    assert ("PUT", "/api/v1/ground-station/wfb") in sites
    assert ("POST", "/api/plugins/{}/enable") in sites
    assert ("DELETE", "/api/v1/ground-station/wfb/pair/{}") in sites


def test_prose_that_merely_mentions_a_route_is_not_a_path() -> None:
    assert all(path != "/api/status" or verb == "GET" for verb, path in _sites())
    assert sum(1 for _, path in _sites() if path == "/api/status") == 1


def test_an_interpolated_asset_path_matches_by_its_literal_prefix() -> None:
    assert ("GET", "/api/plugins/{}/gcs/{}") in _sites()
    assert chk.resolves("/api/plugins/{}/gcs/{}", TABLE)
    assert chk.method_served("/api/plugins/{}/gcs/{}", "GET", TABLE)


def test_a_socket_route_constant_is_checked_by_path_not_as_a_get() -> None:
    sites = _sites()
    assert (None, "/api/v1/ground-station/ws/uplink") in sites
    assert chk.resolves("/api/v1/ground-station/ws/uplink", TABLE)
    # The base constant itself is a path-only reference too.
    assert (None, "/api/v1/ground-station") in sites


def test_a_route_the_agent_does_not_serve_still_fails() -> None:
    assert ("GET", "/api/v1/ground-station/radar") in _sites()
    assert not chk.resolves("/api/v1/ground-station/radar", TABLE)
    assert not chk.method_served("/api/v1/ground-station/wfb", "DELETE", TABLE)
