"""HTTP access to the native control front and the residual Python handler.

Two transports issue the *identical* request so their responses can be diffed:

* the native control front over TCP (the LAN front door, default
  ``http://localhost:8080``);
* the residual Python over its internal unix socket (no key, reached directly so
  the comparison sees the Python handler's own bytes, not a proxied copy of the
  native one), default ``/run/ados/api-internal.sock``.

Both speak the same surface: ``GET`` / ``POST`` / ``PUT`` / ``DELETE`` with
headers and an optional body, plus a streaming read that returns the first N
server-sent-event frames under a deadline. Every request is bounded by a short
timeout so the harness can never hang.

``Probe`` is the small immutable record a request returns: the status code, the
response headers, and the body (bytes for a normal read, or a list of decoded
SSE frames for a streaming read). The ``ok`` flag is false when the transport
could not be reached at all, so an unreachable side is a distinct outcome from a
reachable side that returned an error status.

The ``Clients`` holder keeps already-built ``httpx.Client`` objects so a test can
inject clients backed by an ``httpx.MockTransport`` and exercise the whole
comparison deterministically without a live service.
"""

from __future__ import annotations

import time
from dataclasses import dataclass, field

import httpx

# Per-request bound. The harness is a deterministic dry check, never a soak; a
# slow or absent endpoint must surface as a reachability miss, not a hang.
DEFAULT_TIMEOUT_S = 5.0

# How many SSE frames a streaming read collects before it returns, and the hard
# wall-clock deadline for collecting them (so a quiet stream returns what it has
# rather than blocking to the per-request timeout repeatedly).
DEFAULT_SSE_FRAMES = 3
DEFAULT_SSE_DEADLINE_S = 5.0

# A stand-in authority for unix-socket requests: httpx needs a syntactically
# valid http URL even though the socket transport ignores the host.
_UDS_BASE = "http://api.local"


@dataclass(frozen=True)
class Probe:
    """The result of one request to one transport.

    ``ok`` is false only when the transport could not be reached (a connection or
    transport error), which is distinct from a reachable endpoint returning a
    4xx/5xx — those carry their real ``status`` and ``body``. ``frames`` is the
    decoded SSE frame list for a streaming read; ``body`` is the raw bytes for a
    normal read.
    """

    ok: bool
    status: int = 0
    headers: dict[str, str] = field(default_factory=dict)
    body: bytes = b""
    frames: list[str] = field(default_factory=list)
    error: str | None = None


def _do_request(
    client: httpx.Client,
    method: str,
    path: str,
    headers: dict[str, str] | None,
    body: bytes | None,
    content_type: str | None,
) -> Probe:
    """Issue one bounded request, folding every transport error into a Probe.

    A reachable endpoint's status/headers/body are returned verbatim (including
    an error status); only an unreachable transport yields ``ok=False``.
    """
    req_headers = dict(headers or {})
    if content_type and body is not None:
        req_headers.setdefault("content-type", content_type)
    try:
        resp = client.request(method, path, headers=req_headers, content=body)
    except (httpx.HTTPError, OSError) as exc:
        return Probe(ok=False, error=f"{type(exc).__name__}: {exc}")
    return Probe(
        ok=True,
        status=resp.status_code,
        headers={k.lower(): v for k, v in resp.headers.items()},
        body=resp.content,
    )


def _do_stream(
    client: httpx.Client,
    method: str,
    path: str,
    headers: dict[str, str] | None,
    body: bytes | None,
    content_type: str | None,
    max_frames: int,
    deadline_s: float,
) -> Probe:
    """Read up to ``max_frames`` SSE frames under a wall-clock deadline.

    Frames are split on the blank-line separator the SSE wire format uses. The
    read stops at whichever comes first: ``max_frames`` collected or the deadline
    elapsed. A non-200 stream returns its status with no frames (an error page is
    not a frame sequence), so the comparator sees the status mismatch directly.
    """
    req_headers = dict(headers or {})
    req_headers.setdefault("accept", "text/event-stream")
    if content_type and body is not None:
        req_headers.setdefault("content-type", content_type)
    frames: list[str] = []
    buffer = ""
    deadline = time.monotonic() + deadline_s
    try:
        with client.stream(
            method, path, headers=req_headers, content=body
        ) as resp:
            status = resp.status_code
            resp_headers = {k.lower(): v for k, v in resp.headers.items()}
            if status != 200:
                return Probe(ok=True, status=status, headers=resp_headers)
            for chunk in resp.iter_text():
                buffer += chunk
                while "\n\n" in buffer:
                    raw, buffer = buffer.split("\n\n", 1)
                    if raw.strip():
                        frames.append(raw)
                        if len(frames) >= max_frames:
                            return Probe(
                                ok=True,
                                status=status,
                                headers=resp_headers,
                                frames=frames,
                            )
                if time.monotonic() >= deadline:
                    break
    except (httpx.HTTPError, OSError) as exc:
        return Probe(ok=False, error=f"{type(exc).__name__}: {exc}")
    return Probe(ok=True, status=status, headers=resp_headers, frames=frames)


class Clients:
    """Paired bounded access to the native front and the residual Python.

    Either client may be ``None`` when that surface is not configured for a run,
    in which case a request against it returns an unreachable ``Probe`` rather
    than raising — so a partial run still produces a report.
    """

    def __init__(
        self,
        native_client: httpx.Client | None,
        python_client: httpx.Client | None,
        timeout: float = DEFAULT_TIMEOUT_S,
        sse_frames: int = DEFAULT_SSE_FRAMES,
        sse_deadline: float = DEFAULT_SSE_DEADLINE_S,
    ) -> None:
        self._native = native_client
        self._python = python_client
        self._timeout = timeout
        self._sse_frames = sse_frames
        self._sse_deadline = sse_deadline

    @classmethod
    def connect(
        cls,
        front_base: str | None,
        python_uds: str | None,
        timeout: float = DEFAULT_TIMEOUT_S,
        sse_frames: int = DEFAULT_SSE_FRAMES,
        sse_deadline: float = DEFAULT_SSE_DEADLINE_S,
    ) -> Clients:
        """Build paired clients with real transports: a TCP client to the native
        front and a unix-socket client to the residual Python. Construction never
        performs I/O, so it cannot fail on an absent endpoint; misses surface at
        request time as an unreachable ``Probe``."""
        native = (
            httpx.Client(base_url=front_base, timeout=timeout)
            if front_base
            else None
        )
        python = None
        if python_uds:
            transport = httpx.HTTPTransport(uds=python_uds)
            python = httpx.Client(
                transport=transport, base_url=_UDS_BASE, timeout=timeout
            )
        return cls(native, python, timeout, sse_frames, sse_deadline)

    def close(self) -> None:
        """Close both clients (idempotent)."""
        if self._native is not None:
            self._native.close()
        if self._python is not None:
            self._python.close()

    def __enter__(self) -> Clients:
        return self

    def __exit__(self, *_exc: object) -> None:
        self.close()

    def request_native(
        self,
        method: str,
        path: str,
        headers: dict[str, str] | None = None,
        body: bytes | None = None,
        content_type: str | None = None,
    ) -> Probe:
        """Issue a request to the native control front (TCP)."""
        if self._native is None:
            return Probe(ok=False, error="native front not configured")
        return _do_request(self._native, method, path, headers, body, content_type)

    def request_python(
        self,
        method: str,
        path: str,
        headers: dict[str, str] | None = None,
        body: bytes | None = None,
        content_type: str | None = None,
    ) -> Probe:
        """Issue a request to the residual Python handler (unix socket)."""
        if self._python is None:
            return Probe(ok=False, error="python socket not configured")
        return _do_request(self._python, method, path, headers, body, content_type)

    def stream_native(
        self,
        method: str,
        path: str,
        headers: dict[str, str] | None = None,
        body: bytes | None = None,
        content_type: str | None = None,
    ) -> Probe:
        """Read the first N SSE frames from the native front under the deadline."""
        if self._native is None:
            return Probe(ok=False, error="native front not configured")
        return _do_stream(
            self._native,
            method,
            path,
            headers,
            body,
            content_type,
            self._sse_frames,
            self._sse_deadline,
        )

    def stream_python(
        self,
        method: str,
        path: str,
        headers: dict[str, str] | None = None,
        body: bytes | None = None,
        content_type: str | None = None,
    ) -> Probe:
        """Read the first N SSE frames from the residual Python under the deadline."""
        if self._python is None:
            return Probe(ok=False, error="python socket not configured")
        return _do_stream(
            self._python,
            method,
            path,
            headers,
            body,
            content_type,
            self._sse_frames,
            self._sse_deadline,
        )
