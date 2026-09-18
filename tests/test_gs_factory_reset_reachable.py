"""The ground-station factory reset must be reachable by somebody.

Its authorization gate demanded a captive-portal token from every caller that
was not a loopback TCP peer. Nothing in the tree calls
`CaptiveTokenStore.generate()`, so the store is always empty and `consume()` can
only return False — and when the native front owns the LAN port the residual app
is reached over a Unix socket, where `request.client` is `None` and so not
loopback either. The route was therefore closed to every caller, and no native
route serves this path, so a ground station had no factory reset at all.

The fingerprint confirm is the destructive-action gate and is unchanged; these
tests assert only which callers get as far as it.
"""

from __future__ import annotations

import pytest
from fastapi import HTTPException

from ados.api.routes import ground_station as _gs
from ados.api.routes.ground_station.ui import post_factory_reset

_FINGERPRINT = "ab12cd34ef56"


class _FakeRequest:
    """The two attributes the authorization gate reads."""

    def __init__(self, host: str | None, headers: dict[str, str] | None = None) -> None:
        self.client = None if host is None else type("C", (), {"host": host})()
        self.headers = headers or {}


@pytest.fixture(autouse=True)
def _gs_stubs(monkeypatch):
    class _Pair:
        async def status(self, _kind: str) -> dict:
            return {"fingerprint": _FINGERPRINT}

        async def factory_reset(self, _kind: str) -> dict:
            raise AssertionError("no test here may reach the wipe")

    monkeypatch.setattr(_gs, "_require_ground_profile", lambda: None)
    monkeypatch.setattr(_gs, "_pair_manager", lambda: _Pair())


async def _reset(request, confirm: str) -> HTTPException:
    with pytest.raises(HTTPException) as exc:
        await post_factory_reset(request, confirm=confirm)
    return exc.value


def _code(exc: HTTPException) -> str:
    return exc.detail["error"]["code"]


async def test_a_front_proxied_request_reaches_the_confirm_gate() -> None:
    # The shape the native front produces: proxied over
    # /run/ados/api-internal.sock, so there is no peer address at all. This used
    # to 401 before any confirm was considered, which is what made the route
    # unreachable in the shipped topology.
    exc = await _reset(_FakeRequest(host=None), confirm="wrong")
    assert exc.status_code == 400
    assert _code(exc) == "E_CONFIRM_MISMATCH"


async def test_loopback_still_reaches_the_confirm_gate() -> None:
    exc = await _reset(_FakeRequest(host="127.0.0.1"), confirm="wrong")
    assert exc.status_code == 400
    assert _code(exc) == "E_CONFIRM_MISMATCH"


async def test_a_lan_caller_without_a_captive_token_is_still_refused() -> None:
    # Unchanged, and the point of the gate: a curl from the operator's laptop
    # gets nowhere near the wipe, even with a correct-looking confirm.
    exc = await _reset(_FakeRequest(host="192.168.1.40"), confirm=_FINGERPRINT)
    assert exc.status_code == 401
    assert _code(exc) == "E_CAPTIVE_TOKEN_INVALID"


async def test_a_lan_caller_with_a_live_captive_token_reaches_the_confirm_gate() -> None:
    from ados.services.setup_webapp.captive_token import get_captive_token_store

    token = get_captive_token_store().generate()
    request = _FakeRequest(host="192.168.4.23", headers={"x-ados-captive-key": token})

    exc = await _reset(request, confirm="wrong")
    assert exc.status_code == 400
    assert _code(exc) == "E_CONFIRM_MISMATCH"

    # Single-use: the same token must not open the route twice.
    replay = _FakeRequest(host="192.168.4.23", headers={"x-ados-captive-key": token})
    exc = await _reset(replay, confirm="wrong")
    assert exc.status_code == 401
    assert _code(exc) == "E_CAPTIVE_TOKEN_INVALID"
