"""Tests for token bucket rate limiter."""

from __future__ import annotations

import time
from unittest.mock import patch

import pytest
from starlette.testclient import TestClient

from ados.security.rate_limit import RateLimiter, RateLimitMiddleware


def test_allow_within_burst():
    limiter = RateLimiter(rate=10.0, burst=5)
    for _ in range(5):
        assert limiter.allow("client-1") is True


def test_reject_over_burst():
    limiter = RateLimiter(rate=10.0, burst=3)
    for _ in range(3):
        limiter.allow("client-1")
    assert limiter.allow("client-1") is False


def test_separate_keys():
    limiter = RateLimiter(rate=10.0, burst=2)
    limiter.allow("a")
    limiter.allow("a")
    assert limiter.allow("a") is False
    # Different key should still have tokens
    assert limiter.allow("b") is True


def test_refill_over_time():
    limiter = RateLimiter(rate=100.0, burst=5)
    # Drain the bucket
    for _ in range(5):
        limiter.allow("x")
    assert limiter.allow("x") is False

    # Simulate time passing (refill)
    bucket = limiter._buckets["x"]
    bucket.last_refill -= 1.0  # 1 second ago -> 100 tokens refilled
    assert limiter.allow("x") is True


def test_reset_specific_key():
    limiter = RateLimiter(rate=10.0, burst=2)
    limiter.allow("a")
    limiter.allow("a")
    assert limiter.allow("a") is False

    limiter.reset("a")
    assert limiter.allow("a") is True


def test_reset_all():
    limiter = RateLimiter(rate=10.0, burst=1)
    limiter.allow("a")
    limiter.allow("b")
    assert limiter.allow("a") is False

    limiter.reset()
    assert limiter.allow("a") is True
    assert limiter.allow("b") is True


def test_properties():
    limiter = RateLimiter(rate=5.0, burst=10)
    assert limiter.rate == 5.0
    assert limiter.burst == 10


def test_middleware_allows_requests():
    from starlette.applications import Starlette
    from starlette.responses import PlainTextResponse
    from starlette.routing import Route

    async def homepage(request):
        return PlainTextResponse("ok")

    app = Starlette(routes=[Route("/", homepage)])
    app.add_middleware(RateLimitMiddleware, rate=100.0, burst=50)

    client = TestClient(app)
    resp = client.get("/")
    assert resp.status_code == 200


def test_middleware_rejects_excess():
    from starlette.applications import Starlette
    from starlette.responses import PlainTextResponse
    from starlette.routing import Route

    async def homepage(request):
        return PlainTextResponse("ok")

    app = Starlette(routes=[Route("/", homepage)])
    app.add_middleware(RateLimitMiddleware, rate=0.001, burst=1)

    client = TestClient(app)
    # First request OK
    resp = client.get("/")
    assert resp.status_code == 200

    # Second should be rate-limited
    resp = client.get("/")
    assert resp.status_code == 429
    assert "Too many requests" in resp.json()["detail"]
