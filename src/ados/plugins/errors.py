"""Exception types for the plugin subsystem.

Kept in their own module so callers can import them without pulling
in the rest of the plugin machinery.
"""

from __future__ import annotations


class PluginError(Exception):
    """Base class for plugin-system errors."""


class ManifestError(PluginError):
    """Raised when a manifest fails to load, parse, or validate."""


class SignatureError(PluginError):
    """Raised when an archive signature is missing, malformed, or invalid.

    Sub-classified so the CLI can map to the right exit code:

    * ``MISSING``: archive is unsigned and the install path requires signed.
    * ``INVALID``: signature does not verify against any trusted key.
    * ``REVOKED``: signature verifies but the signer key is on the revocation list.
    * ``UNKNOWN_SIGNER``: signature shape is valid but the signer id is not on
      the trusted-keys list.
    """

    KIND_MISSING = "missing"
    KIND_INVALID = "invalid"
    KIND_REVOKED = "revoked"
    KIND_UNKNOWN_SIGNER = "unknown_signer"

    def __init__(self, kind: str, message: str) -> None:
        super().__init__(message)
        self.kind = kind


class ArchiveError(PluginError):
    """Raised on malformed ``.adosplug`` archives (bad zip, missing manifest,
    path-traversal entries, oversized payload)."""


class SupervisorError(PluginError):
    """Raised on lifecycle transitions that are illegal or fail to apply.

    Examples: enabling an uninstalled plugin, removing a plugin while it is
    still running, systemd unit write fails, cgroup slice not present.
    """


class CapabilityDenied(PluginError):
    """Raised when a plugin attempts a capability it did not declare or that
    the operator has revoked. Always wraps a structured event for the events
    log so the GCS detail page can show a meaningful denial."""

    def __init__(self, plugin_id: str, capability: str) -> None:
        super().__init__(
            f"plugin {plugin_id} attempted capability {capability} without grant"
        )
        self.plugin_id = plugin_id
        self.capability = capability
