"""A bare /cockpit must not drop its query string on the way to /cockpit/.

The cockpit reads its access key off the URL, so dropping the query silently
broke every `/cockpit?key=...` reach link: the operator landed on a page asking
to be paired again, with nothing on screen to explain why. The kiosk's
render-profile flag travels the same way and was lost for the same reason.
"""

from __future__ import annotations

import pytest
from fastapi.testclient import TestClient

from ados.api.server import create_app
from tests.api_runtime_utils import build_api_runtime


@pytest.fixture
def client() -> TestClient:
    return TestClient(create_app(build_api_runtime()), follow_redirects=False)


def test_the_access_key_survives_the_redirect(client: TestClient) -> None:
    resp = client.get("/cockpit?key=abc123")
    assert resp.status_code in (301, 302, 307, 308)
    assert resp.headers["location"] == "/cockpit/?key=abc123"


def test_the_render_profile_flag_survives_the_redirect(client: TestClient) -> None:
    resp = client.get("/cockpit?layer=minimal")
    assert resp.headers["location"] == "/cockpit/?layer=minimal"


def test_several_parameters_all_survive(client: TestClient) -> None:
    resp = client.get("/cockpit?layer=minimal&key=abc123&demo=1")
    assert resp.headers["location"] == "/cockpit/?layer=minimal&key=abc123&demo=1"


def test_a_bare_cockpit_still_redirects_cleanly(client: TestClient) -> None:
    # No query means no trailing '?', so the common case stays a clean URL.
    resp = client.get("/cockpit")
    assert resp.headers["location"] == "/cockpit/"
