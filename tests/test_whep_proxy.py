"""Tests for the WHEP reverse-proxy at the root path.

The proxy forwards POST /whep, DELETE /whep/{id}, and PATCH /whep/{id}
to the local MediaMTX instance. Tests use ``httpx.MockTransport`` to
intercept outbound requests so the suite never needs a real MediaMTX.
"""

from __future__ import annotations

from collections.abc import Callable
from typing import Any

import httpx
import pytest
from fastapi.testclient import TestClient

from ados.api.routes import whep as whep_module
from ados.api.server import create_app
from ados.core.config import ADOSConfig
from tests.api_runtime_utils import build_api_runtime

# Aliases keep the fixture and test signatures readable. ``UpstreamHandler``
# turns an httpx.Request into the httpx.Response a real MediaMTX would
# send. ``InstallUpstream`` is the fixture-returned factory: callers pass
# a handler, get back the list that captures every forwarded request.
UpstreamHandler = Callable[[httpx.Request], httpx.Response]
InstallUpstream = Callable[[UpstreamHandler], list[httpx.Request]]


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


def _build_runtime(profile: str = "ground_station") -> Any:
    cfg = ADOSConfig()
    cfg.agent.profile = profile
    return build_api_runtime(config=cfg)


@pytest.fixture
def ground_runtime() -> Any:
    return _build_runtime("ground_station")


@pytest.fixture
def drone_runtime() -> Any:
    return _build_runtime("auto")


@pytest.fixture
def ground_client(ground_runtime: Any) -> TestClient:
    return TestClient(create_app(ground_runtime))


@pytest.fixture
def drone_client(drone_runtime: Any) -> TestClient:
    return TestClient(create_app(drone_runtime))


@pytest.fixture
def install_mock_upstream(monkeypatch: pytest.MonkeyPatch) -> InstallUpstream:
    """Install a MockTransport-backed httpx.AsyncClient.

    Returns a factory that, when given a handler, swaps the WHEP
    module's singleton client for one routed through MockTransport
    and yields a list that captures every request the handler sees.
    """

    def _install(handler: UpstreamHandler) -> list[httpx.Request]:
        captured: list[httpx.Request] = []

        def _wrapper(request: httpx.Request) -> httpx.Response:
            captured.append(request)
            return handler(request)

        transport = httpx.MockTransport(_wrapper)
        client = httpx.AsyncClient(transport=transport)
        # Ensure the proxy reaches for our client, not whatever the
        # process-wide default would otherwise lazily build.
        monkeypatch.setattr(whep_module, "_client_singleton", client)
        return captured

    return _install


# ---------------------------------------------------------------------------
# Happy-path: POST offer/answer
# ---------------------------------------------------------------------------


def test_post_whep_returns_sdp_answer_with_location(
    ground_client: TestClient,
    install_mock_upstream: InstallUpstream,
) -> None:
    """POST /whep forwards an SDP offer and returns 201 + SDP answer + Location."""
    answer_sdp = b"v=0\r\no=- 1 1 IN IP4 0.0.0.0\r\ns=-\r\n"

    def _handler(request: httpx.Request) -> httpx.Response:
        # Verify the upstream request is exactly what MediaMTX expects
        assert request.method == "POST"
        assert str(request.url) == "http://127.0.0.1:8889/main/whep"
        # Body passes through verbatim
        assert request.content == b"v=0\r\no=- 0 0 IN IP4 1.1.1.1\r\ns=-\r\n"
        # Content-Type passes through verbatim
        assert request.headers.get("content-type") == "application/sdp"
        return httpx.Response(
            201,
            headers={
                "content-type": "application/sdp",
                "location": "/main/whep/abc-123",
            },
            content=answer_sdp,
        )

    captured = install_mock_upstream(_handler)

    resp = ground_client.post(
        "/whep",
        content=b"v=0\r\no=- 0 0 IN IP4 1.1.1.1\r\ns=-\r\n",
        headers={"content-type": "application/sdp"},
    )

    assert resp.status_code == 201
    assert resp.content == answer_sdp
    assert resp.headers["content-type"] == "application/sdp"
    # Location header is forwarded unmodified so the Android client can
    # dereference it back through the proxy.
    assert resp.headers["location"] == "/main/whep/abc-123"
    assert len(captured) == 1


# ---------------------------------------------------------------------------
# PATCH: ICE restart with trickle SDP fragment
# ---------------------------------------------------------------------------


def test_patch_whep_ice_restart_passes_through_content_type(
    ground_client: TestClient,
    install_mock_upstream: InstallUpstream,
) -> None:
    """PATCH /whep/{id} forwards the trickle-ICE Content-Type."""

    def _handler(request: httpx.Request) -> httpx.Response:
        assert request.method == "PATCH"
        assert str(request.url) == "http://127.0.0.1:8889/main/whep/abc-123"
        assert (
            request.headers.get("content-type")
            == "application/trickle-ice-sdpfrag"
        )
        assert request.content == b"a=ice-ufrag:foo\r\n"
        return httpx.Response(204)

    captured = install_mock_upstream(_handler)

    resp = ground_client.patch(
        "/whep/abc-123",
        content=b"a=ice-ufrag:foo\r\n",
        headers={"content-type": "application/trickle-ice-sdpfrag"},
    )

    assert resp.status_code == 204
    assert len(captured) == 1


# ---------------------------------------------------------------------------
# DELETE: terminate session
# ---------------------------------------------------------------------------


def test_delete_whep_terminates_session(
    ground_client: TestClient,
    install_mock_upstream: InstallUpstream,
) -> None:
    """DELETE /whep/{id} forwards a session termination."""

    def _handler(request: httpx.Request) -> httpx.Response:
        assert request.method == "DELETE"
        assert str(request.url) == "http://127.0.0.1:8889/main/whep/abc-123"
        return httpx.Response(200)

    captured = install_mock_upstream(_handler)

    resp = ground_client.delete("/whep/abc-123")

    assert resp.status_code == 200
    assert len(captured) == 1


# ---------------------------------------------------------------------------
# Profile gate
# ---------------------------------------------------------------------------


def test_profile_gate_drone_profile_returns_404(
    drone_client: TestClient,
    install_mock_upstream: InstallUpstream,
) -> None:
    """Drone profile gets 404 with the canonical profile-mismatch code."""

    def _handler(request: httpx.Request) -> httpx.Response:
        # The proxy must short-circuit before reaching the upstream.
        raise AssertionError("upstream contacted on drone profile")

    captured = install_mock_upstream(_handler)

    resp = drone_client.post(
        "/whep",
        content=b"v=0\r\n",
        headers={"content-type": "application/sdp"},
    )

    assert resp.status_code == 404
    body = resp.json()
    assert body["detail"]["error"]["code"] == "E_PROFILE_MISMATCH"
    assert captured == []


# ---------------------------------------------------------------------------
# Upstream errors bubble up
# ---------------------------------------------------------------------------


def test_upstream_5xx_bubbles_to_client(
    ground_client: TestClient,
    install_mock_upstream: InstallUpstream,
) -> None:
    """A 5xx from MediaMTX is forwarded to the caller without translation."""

    def _handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(
            502,
            headers={"content-type": "text/plain"},
            content=b"upstream stream not publishing",
        )

    install_mock_upstream(_handler)

    resp = ground_client.post(
        "/whep",
        content=b"v=0\r\n",
        headers={"content-type": "application/sdp"},
    )

    assert resp.status_code == 502
    assert resp.content == b"upstream stream not publishing"


def test_upstream_unreachable_returns_503(
    ground_client: TestClient,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """When MediaMTX is down, the proxy returns 503 instead of 500."""

    def _refuse(request: httpx.Request) -> httpx.Response:
        raise httpx.ConnectError("connection refused", request=request)

    transport = httpx.MockTransport(_refuse)
    client = httpx.AsyncClient(transport=transport)
    monkeypatch.setattr(whep_module, "_client_singleton", client)

    resp = ground_client.post(
        "/whep",
        content=b"v=0\r\n",
        headers={"content-type": "application/sdp"},
    )

    assert resp.status_code == 503
