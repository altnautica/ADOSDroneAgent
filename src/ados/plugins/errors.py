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
    """Raised when an archive's ``SIGNATURE`` entry is malformed.

    Verifying the signature against trusted keys is the native plugin
    host's job; the Python parser only checks the entry's shape.
    """

    KIND_INVALID = "invalid"

    def __init__(self, kind: str, message: str) -> None:
        super().__init__(message)
        self.kind = kind


class ArchiveError(PluginError):
    """Raised on malformed ``.adosplug`` archives (bad zip, missing manifest,
    path-traversal entries, oversized payload)."""


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
