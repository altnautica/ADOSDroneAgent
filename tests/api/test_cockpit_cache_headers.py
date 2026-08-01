"""The cockpit entry must revalidate; its content-hashed assets need not.

A browser that keeps `index.html` keeps loading the bundle that file names, so
an update is invisible: the page still renders and nothing looks stale, while
the panel runs code the node stopped serving. The asset names carry a content
hash and change when their bytes do, so those are safe to keep forever — it is
only the entry, whose name never changes, that has to be checked each time.

This was found the hard way. A node that refused to serve its own page left a
browser holding an old copy with no way to fetch a newer one, and the stale copy
was where the fault lived.
"""

from __future__ import annotations

import pytest
from fastapi.testclient import TestClient

from ados.api.server import create_app
from tests.api_runtime_utils import build_api_runtime


@pytest.fixture
def client() -> TestClient:
    return TestClient(create_app(build_api_runtime()))


def test_the_entry_is_revalidated_every_time(client: TestClient) -> None:
    res = client.get("/cockpit/")
    assert res.status_code == 200
    assert res.headers.get("cache-control") == "no-cache"


def test_a_hashed_asset_may_be_kept(client: TestClient) -> None:
    # Find whatever the current build emitted rather than pinning a hash.
    entry = client.get("/cockpit/").text
    asset = next(
        (
            part.split('"')[0]
            for part in entry.split('src="/cockpit/')[1:]
            if part.split('"')[0].endswith(".js")
        ),
        None,
    )
    assert asset, "the cockpit entry names no script; the bundle is not staged"

    res = client.get(f"/cockpit/{asset}")
    assert res.status_code == 200
    cache = res.headers.get("cache-control", "")
    assert "immutable" in cache, cache
    # Long enough to be worth having; the name changes when the bytes do.
    assert "max-age=31536000" in cache, cache
