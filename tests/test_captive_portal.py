"""The captive portal sends every OS probe and every page to the setup webapp."""

from __future__ import annotations

import http.client
import threading
from http.server import ThreadingHTTPServer

import pytest

from ados.services.setup_webapp.captive_dns import _ProbeHandler, setup_url


@pytest.fixture
def portal():
    handler = type("_Handler", (_ProbeHandler,), {"target_url": setup_url(8080)})
    httpd = ThreadingHTTPServer(("127.0.0.1", 0), handler)
    thread = threading.Thread(target=httpd.serve_forever, daemon=True)
    thread.start()
    try:
        yield httpd.server_address[1]
    finally:
        httpd.shutdown()
        httpd.server_close()


def _get(port: int, path: str, method: str = "GET") -> http.client.HTTPResponse:
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
    conn.request(method, path)
    resp = conn.getresponse()
    resp.read()
    conn.close()
    return resp


@pytest.mark.parametrize(
    "path", ["/generate_204", "/hotspot-detect.html", "/connecttest.txt", "/", "/anything"]
)
def test_every_request_redirects_to_the_setup_webapp(portal, path):
    resp = _get(portal, path)

    # A 204 on /generate_204 tells Android the network is validated and
    # suppresses the sign-in sheet; the portal must never answer it.
    assert resp.status == 302
    assert resp.getheader("Location") == "http://192.168.4.1:8080/"


def test_the_redirect_target_is_never_the_portal_itself(portal):
    resp = _get(portal, "/", method="HEAD")

    location = resp.getheader("Location")
    assert location is not None
    assert location.startswith("http://192.168.4.1:8080/")
    assert location not in ("http://192.168.4.1/", "http://192.168.4.1:80/")
