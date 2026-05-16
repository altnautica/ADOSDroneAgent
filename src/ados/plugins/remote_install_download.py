"""Download path for the cloud-relay plugin install receiver.

Lives in its own module so the security defenses (URL allowlist,
streaming size cap, optional SHA256 verify) are easy to audit and so
the receiver module stays under the soft 500-LOC cap.

The ``signedUrl`` value comes from a Convex command-queue row. A
compromised row, a tampered relay, or an MITM on the cloud transport
could redirect the agent to attacker-controlled HTTPS, plain HTTP, or
a multi-gigabyte body. The defenses here run before any byte of the
body is trusted:

* :func:`validate_download_url` — scheme must be ``https``, hostname
  must end with an allowlisted suffix.
* :func:`stream_download` — body buffer is capped at
  :data:`DOWNLOAD_MAX_BYTES`. Exceeding bodies abort mid-stream.
* :func:`verify_sha256` — optional manifest-hash check when the
  command row declares one. Defense-in-depth on top of the Ed25519
  signature verify that runs later in the supervisor.
"""

from __future__ import annotations

import hashlib
from urllib.parse import urlparse

import httpx


class DownloadError(Exception):
    """Raised when a cloud-relay archive download is refused or aborted.

    Distinct from ``SupervisorError`` / ``SignatureError`` so the
    receiver can classify pre-signature transport failures separately
    from on-disk install failures.
    """


# Hard cap on a downloaded archive body. Double the archive.py cap so
# the download layer rejects oversized payloads before the supervisor
# even sees them. Sized in bytes so the streaming guard does an exact
# byte comparison.
DOWNLOAD_MAX_BYTES = 100 * 1024 * 1024


# Allowlist of acceptable hostname suffixes for ``signedUrl`` downloads.
# Reject any host not ending in one of these. Module-level constant so
# the policy is auditable in one place and the test suite can monkey-
# patch it without reaching into private helpers.
CONVEX_HOST_SUFFIXES: tuple[str, ...] = (
    ".convex.cloud",
    ".convex.altnautica.com",
    "localhost",
)


def validate_download_url(url: str) -> None:
    """Reject URLs that escape the Convex / dev-mode allowlist.

    Three independent checks, in order: scheme must be ``https``,
    hostname must be present, hostname must end with one of the
    allowlisted suffixes (suffix match on the labelled host, not a
    substring search). Raises :class:`DownloadError` with a short
    reason on any failure so the caller can surface it on the
    sidecar.
    """
    if not url:
        raise DownloadError("download url is empty")
    try:
        parsed = urlparse(url)
    except ValueError as exc:
        raise DownloadError(f"download url is not parseable: {exc}") from exc
    scheme = (parsed.scheme or "").lower()
    if scheme != "https":
        raise DownloadError("non-https url rejected")
    host = (parsed.hostname or "").lower()
    if not host:
        raise DownloadError("download url has no host")
    # Exact-match for ``localhost`` (dev mode) avoids a substring
    # match against, e.g., ``evil.localhost.example.com``.
    if host == "localhost":
        return
    for suffix in CONVEX_HOST_SUFFIXES:
        if suffix == "localhost":
            continue
        if host.endswith(suffix):
            return
    raise DownloadError(f"download host {host!r} is not on the allowlist")


async def stream_download(
    client: httpx.AsyncClient, url: str
) -> tuple[int, bytes]:
    """Stream a GET response and accumulate up to the size cap.

    Returns ``(status_code, body_bytes)``. Body is empty on a non-200
    response — caller decides how to react. On a 200 that exceeds
    :data:`DOWNLOAD_MAX_BYTES` the stream is aborted and a
    :class:`DownloadError` raised so the agent does not buffer a
    10 GB payload in RAM.
    """
    async with client.stream("GET", url) as resp:
        if resp.status_code != 200:
            # Drain any body so the connection can be reused. Body
            # bytes are not surfaced for non-200; the caller only
            # inspects the status code.
            try:
                await resp.aread()
            except httpx.HTTPError:
                pass
            return resp.status_code, b""
        buf = bytearray()
        async for chunk in resp.aiter_bytes():
            buf.extend(chunk)
            if len(buf) > DOWNLOAD_MAX_BYTES:
                raise DownloadError("size cap exceeded")
        return 200, bytes(buf)


def verify_sha256(body: bytes, expected_hex: str) -> None:
    """Compare the downloaded body's SHA256 to a manifest-declared hash.

    Case-insensitive hex compare. Raises :class:`DownloadError` on
    mismatch so the caller can surface it on the install sidecar.
    Empty ``expected_hex`` is a no-op — the cloud command row did not
    declare a hash and we fall back to the supervisor's Ed25519
    signature check.
    """
    if not expected_hex:
        return
    actual = hashlib.sha256(body).hexdigest()
    if actual.lower() != expected_hex.lower():
        raise DownloadError(
            f"sha256 mismatch: expected {expected_hex}, got {actual}"
        )


__all__ = [
    "CONVEX_HOST_SUFFIXES",
    "DOWNLOAD_MAX_BYTES",
    "DownloadError",
    "stream_download",
    "validate_download_url",
    "verify_sha256",
]
