"""Helper for installing a plugin from a remote ``.adosplug`` URL.

This module backs the ``POST /api/plugins/install_from_url`` REST endpoint.
The endpoint exists so a paired GCS can hand the agent a published
archive URL (typically a GitHub release asset hosted by the canonical
extensions repo) and have the agent download + verify + install it
directly, with no intermediate storage hop.

The integrity contract is exactly the same as the local-multipart
``/install`` endpoint once the bytes are on disk: archive parse,
signature verify, manifest validation, board compatibility, permission
gating, and systemd unit installation are all delegated to
:meth:`PluginSupervisor.install_archive`. This file owns only the
transport defenses that run before the supervisor sees the bytes:

* URL scheme is ``https``.
* Host is on an allowlist that ships with sensible defaults
  (``github.com``, ``objects.githubusercontent.com``, ``amazonaws.com``
  subdomains, plus a configurable registry host).
* Total body capped at :data:`MAX_PLUGIN_ARCHIVE_SIZE`.
* Optional SHA-256 pin compared against the streamed bytes — the
  registry publishes the hash so the GCS can pass it through.

NOTE: there is intentional overlap between this module and
``ados.plugins.remote_install._download_with_refresh``. The cloud-relay
path adds a signed-URL refresh ladder and the Convex host allowlist;
this REST path is a one-shot download from a public release asset.
A future cleanup pass can dedupe both into one streaming downloader
that takes a host policy. Keeping the two paths distinct for now to
avoid coupling the REST shape to the cloud-relay command-queue shape.
"""

from __future__ import annotations

import hashlib
from dataclasses import dataclass
from pathlib import Path
from urllib.parse import urlparse

import httpx

from ados.core.logging import get_logger

log = get_logger("plugins.install_from_url")


# Hard cap on a downloaded ``.adosplug`` body. Mirrors the cloud-relay
# path's :data:`DOWNLOAD_MAX_BYTES` so behaviour matches across
# transports. Sized in bytes so the streaming guard does an exact
# byte comparison.
MAX_PLUGIN_ARCHIVE_SIZE = 100 * 1024 * 1024

# Allowlist of hostname suffixes (or exact hosts) the REST install
# endpoint accepts. The default set covers GitHub release assets
# (which redirect to ``objects.githubusercontent.com``) and S3-hosted
# release artifacts. ``localhost`` is included for dev/test rigs.
# Operators who self-host a registry can extend this via the agent
# config (extension point reserved; the constant is the v1 default).
DEFAULT_ALLOWED_HOSTS: tuple[str, ...] = (
    "github.com",
    "objects.githubusercontent.com",
    "amazonaws.com",
    "localhost",
)

# Timeouts: 60 s to open the connection, 5 min total. Matches the
# operator-visible install dialog progress envelope; longer than the
# cloud-relay 60 s because release-asset downloads can be slower.
DOWNLOAD_CONNECT_TIMEOUT = 60.0
DOWNLOAD_TOTAL_TIMEOUT = 300.0


class UrlValidationError(Exception):
    """Raised when the supplied URL is not on the allowlist."""


class ArchiveDownloadError(Exception):
    """Raised when the archive cannot be retrieved from the URL."""


class ArchiveTooLargeError(Exception):
    """Raised when the streamed body exceeds the size cap."""


class Sha256MismatchError(Exception):
    """Raised when the streamed body does not match the expected hash."""


@dataclass(frozen=True)
class DownloadOutcome:
    """Result of a successful download.

    ``path`` is the on-disk archive ready to hand to the supervisor.
    ``sha256_hex`` is the computed digest of the downloaded bytes,
    surfaced so the REST handler can log / echo it back to the GCS.
    """

    path: Path
    sha256_hex: str
    byte_count: int


def validate_install_url(
    url: str,
    *,
    allowed_hosts: tuple[str, ...] = DEFAULT_ALLOWED_HOSTS,
) -> None:
    """Reject URLs that are not on the allowlist.

    Scheme must be ``https``. Hostname must either equal one of the
    allowlisted entries or end in ``.<entry>`` (suffix match on the
    labelled host, not a substring search). Raises
    :class:`UrlValidationError` with a short reason on any failure.
    """
    if not url:
        raise UrlValidationError("url is empty")
    try:
        parsed = urlparse(url)
    except ValueError as exc:
        raise UrlValidationError(f"url is not parseable: {exc}") from exc
    scheme = (parsed.scheme or "").lower()
    if scheme != "https":
        raise UrlValidationError("only https urls are accepted")
    host = (parsed.hostname or "").lower()
    if not host:
        raise UrlValidationError("url has no host")
    for entry in allowed_hosts:
        entry_lc = entry.lower()
        if host == entry_lc:
            return
        # Suffix match on labelled boundary so a host of
        # ``evil-github.com`` does NOT match an allowlist entry of
        # ``github.com``.
        if host.endswith("." + entry_lc):
            return
    raise UrlValidationError(f"host {host!r} is not on the allowlist")


async def stream_archive_to_path(
    *,
    client: httpx.AsyncClient,
    url: str,
    dest: Path,
    max_bytes: int | None = None,
    expected_sha256: str = "",
) -> DownloadOutcome:
    """Stream ``url`` into ``dest``, enforcing size and (optional) hash.

    Returns a :class:`DownloadOutcome` describing the downloaded bytes
    on success. Raises:

    * :class:`ArchiveDownloadError` on transport failure or non-200
      status.
    * :class:`ArchiveTooLargeError` when the streamed body would
      exceed ``max_bytes``. The stream is aborted partway through.
    * :class:`Sha256MismatchError` when ``expected_sha256`` is supplied
      and does not match the computed digest.

    The caller is responsible for cleaning up ``dest`` on error.
    """
    # Resolve the cap at call time so tests can monkeypatch the
    # module-level constant without having to thread the value through
    # every caller.
    if max_bytes is None:
        max_bytes = MAX_PLUGIN_ARCHIVE_SIZE
    hasher = hashlib.sha256()
    total = 0
    try:
        async with client.stream("GET", url, follow_redirects=True) as resp:
            if resp.status_code != 200:
                # Drain so the connection can be reused. Body bytes
                # are not surfaced.
                try:
                    await resp.aread()
                except httpx.HTTPError:
                    pass
                raise ArchiveDownloadError(
                    f"HTTP {resp.status_code} from {url}"
                )
            with dest.open("wb") as fh:
                async for chunk in resp.aiter_bytes():
                    if not chunk:
                        continue
                    total += len(chunk)
                    if total > max_bytes:
                        raise ArchiveTooLargeError(
                            f"archive exceeds {max_bytes} byte cap"
                        )
                    hasher.update(chunk)
                    fh.write(chunk)
    except (ArchiveTooLargeError, Sha256MismatchError):
        raise
    except httpx.HTTPError as exc:
        raise ArchiveDownloadError(str(exc)) from exc

    digest = hasher.hexdigest()
    if expected_sha256:
        if digest.lower() != expected_sha256.lower():
            raise Sha256MismatchError(
                f"expected {expected_sha256}, got {digest}"
            )
    return DownloadOutcome(path=dest, sha256_hex=digest, byte_count=total)


__all__ = [
    "ArchiveDownloadError",
    "ArchiveTooLargeError",
    "DEFAULT_ALLOWED_HOSTS",
    "DOWNLOAD_CONNECT_TIMEOUT",
    "DOWNLOAD_TOTAL_TIMEOUT",
    "DownloadOutcome",
    "MAX_PLUGIN_ARCHIVE_SIZE",
    "Sha256MismatchError",
    "UrlValidationError",
    "stream_archive_to_path",
    "validate_install_url",
]
