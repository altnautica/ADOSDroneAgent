"""Plugin manifest model.

A plugin manifest is the declarative contract: identity, halves shipped,
capabilities requested, lifecycle settings, compatibility constraints. The
manifest is the only field the host trusts after signature verification;
everything the supervisor and the GCS do is driven from manifest content.

Schema design choices:

* Reverse-DNS ``id`` is enforced at validate time. Squatting on
  short ids is not permitted.
* ``permissions`` is a union: a bare string means "required", an object
  with ``id`` plus ``required: false`` means optional or degradable.
* The ``agent`` and ``gcs`` blocks are both optional. A plugin can ship
  one half, the other, or both.
* Unknown top-level keys are rejected (forbid extra). Unknown nested
  keys under ``extra`` are allowed for vendor-specific extension.

This module produces a :class:`PluginManifest` Pydantic model and is
the source of the JSON Schema the SDK ships and the public docs render.
The schema is exported via :func:`schema_dict` for downstream consumers.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any, Literal

import yaml
from pydantic import Field, field_validator, model_validator

from ados.plugins.capabilities import is_known_agent_capability
from ados.plugins.errors import ManifestError
from ados.plugins.manifest._agent import AgentBlock
from ados.plugins.manifest._agent_parts import (
    AgentContributes,
    DeclaredCapability,
    MavlinkComponent,
    PayloadSpec,
    ResourceLimits,
    ServiceSpec,
    SharedTopic,
    VendorAttribution,
    VisionContribution,
    VisionModelRef,
)
from ados.plugins.manifest._common import (
    ARCH_OS_PATTERN,
    BIN_NAME_PATTERN,
    BIN_PREFIX,
    CONTRIBUTION_ID_PATTERN,
    DECLARED_CAP_NAME_PATTERN,
    PLUGIN_ID_PATTERN,
    PROFILES,
    SEMVER_PATTERN,
    SHA256_HEX_PATTERN,
    SHARED_TOPIC_PATTERN,
    STANDARD_RESOURCE_CEILINGS,
    PermissionRef,
    Profile,
    _plugin_leaf,
    _StrictModel,
    log,
)
from ados.plugins.manifest._gcs import (
    AgentPage,
    GcsBlock,
    GcsContributes,
    NodeSurface,
)


class Compatibility(_StrictModel):
    ados_version: str = Field(..., min_length=1)
    """Semver range, e.g. ``>=0.9.0,<1.0.0``."""

    gcs_version: str | None = None

    supported_boards: list[str] = Field(default_factory=list)
    """HAL board ids this plugin supports.

    Two spellings mean "any board": an empty list (or an absent field), and a
    list containing the literal ``"*"``. The wildcard form is accepted
    because the scaffolder emitted ``supported_boards: ["*"]`` and it was an
    exact-match reject on every real board — the developer's first
    end-to-end install failed on a line the tool itself wrote. Use
    :meth:`supports_board` rather than testing the list directly so both
    forms behave the same everywhere."""

    min_tier: int | None = Field(None, ge=1, le=4)
    """Minimum compute-class tier the plugin needs (1=basic … 4=highest).

    Optional and additive: when absent there is no tier floor and any
    board passes (lenient, matching the ``supported_boards`` empty-list
    behavior). When set, the supervisor refuses install/enable on a board
    whose detected tier is below this value. A board with an unknown tier
    is never blocked, so the gate only bites when both the floor and the
    board tier are known."""

    def supports_board(self, board_id: str) -> bool:
        """True when ``board_id`` satisfies :attr:`supported_boards`."""
        if not self.supported_boards:
            return True
        return "*" in self.supported_boards or board_id in self.supported_boards


class HardwareRequirements(_StrictModel):
    """Optional hardware-side requirements surfaced in the install dialog.

    All fields are free-form so the dialog can render whatever the
    manifest author wants the operator to see. The agent does not enforce
    any of these at install time; they are informational copy."""

    cameras: str | None = None
    fc_firmware: str | None = None
    boards: list[str] = Field(default_factory=list)
    optional: list[str] = Field(default_factory=list)


class ResourceImpact(_StrictModel):
    """Estimated runtime resource impact. The supervisor still enforces
    the hard limits declared under ``agent.resources``; these numbers are
    forecast copy for the install-dialog summary."""

    # CPU peak is allowed up to 1000 so multi-core peak figures
    # (e.g. 4 cores * 100% = 400) parse without truncation.
    cpu_percent_peak: float | None = Field(None, ge=0, le=1000)
    ram_mb: float | None = Field(None, gt=0)
    pids: int | None = Field(None, gt=0)
    startup_time_seconds: float | None = Field(None, gt=0)
    # Steady-state output rate for plugins that push a periodic stream
    # (pose, video frames, sensor samples). Renders in place of CPU peak
    # on the install-dialog resource-impact card when present.
    output_rate_hz: float | None = Field(None, gt=0, le=10000)


class FcParameter(_StrictModel):
    """A single firmware parameter the plugin expects the operator to
    set before the feature behaves correctly. ``value`` is optional
    because some parameters take a bitmask the operator computes from
    multiple flags; in that case ``note`` carries the guidance."""

    param: str = Field(..., min_length=1)
    note: str | None = None
    value: str | float | int | None = None


class RequiredFcParameters(_StrictModel):
    """Per-firmware bucket of required parameter hints. Each bucket is
    optional so a plugin can ship guidance for only the firmware it
    actually targets."""

    ardupilot: list[FcParameter] = Field(default_factory=list)
    px4: list[FcParameter] = Field(default_factory=list)
    inav: list[FcParameter] = Field(default_factory=list)


class Screenshot(_StrictModel):
    """One screenshot entry rendered by the install dialog."""

    url: str = Field(..., min_length=1)
    caption: str | None = None


class PluginManifest(_StrictModel):
    """Top-level plugin manifest. Loaded from ``manifest.yaml``."""

    schema_version: int = Field(1, ge=1, le=3)
    """Manifest schema version. ``1`` is the original baseline. ``2``
    unlocks the additional ``agent`` fields ``vendor_attribution``,
    ``subprocess_spawn``, and ``per_drone_config``. ``3`` unlocks the
    MCP ``tools`` / ``resources`` / ``prompts`` contributions on both
    the agent and GCS halves. Every version parses identical-shape
    older manifests; the version field is informational so older
    tooling can route on schema generation."""
    id: str
    version: str
    name: str
    description: str = ""
    author: str = ""
    homepage: str | None = None
    license: str = ""
    risk: Literal["low", "medium", "high", "critical"] = "medium"

    compatibility: Compatibility
    agent: AgentBlock | None = None
    gcs: GcsBlock | None = None

    # --- Optional install-dialog content fields ---
    # These are informational copy the GCS install modal renders to give
    # the operator a richer pre-install summary. The agent does not
    # enforce or interpret any of them; they are forward-compatible and
    # may be absent on older manifests.
    description_long: str | None = None
    features: list[str] = Field(default_factory=list)
    hardware_requirements: HardwareRequirements | None = None
    resource_impact: ResourceImpact | None = None
    required_fc_parameters: RequiredFcParameters | None = None
    telemetry_fields: list[str] = Field(default_factory=list)
    documentation_url: str | None = None
    screenshots: list[Screenshot] = Field(default_factory=list)

    extra: dict[str, Any] = Field(default_factory=dict)

    @field_validator("id")
    @classmethod
    def _validate_id(cls, v: str) -> str:
        if not PLUGIN_ID_PATTERN.match(v):
            raise ManifestError(
                f"plugin id {v!r} must be reverse-DNS lowercase, e.g. com.example.thermal"
            )
        return v

    @field_validator("version")
    @classmethod
    def _validate_version(cls, v: str) -> str:
        if not SEMVER_PATTERN.match(v):
            raise ManifestError(f"plugin version {v!r} is not valid semver")
        return v

    @field_validator("documentation_url")
    @classmethod
    def _validate_documentation_url(cls, v: str | None) -> str | None:
        if v is None:
            return v
        if not v.startswith("https://"):
            raise ManifestError(
                f"documentation_url must use https://, got {v!r}"
            )
        return v

    @model_validator(mode="after")
    def _at_least_one_half(self) -> PluginManifest:
        if self.agent is None and self.gcs is None:
            raise ManifestError(
                f"plugin {self.id} declares neither agent nor gcs half; "
                "at least one is required"
            )
        return self

    @model_validator(mode="after")
    def _validate_plugin_namespace(self) -> PluginManifest:
        """Declared capabilities and shared topics live under the plugin's
        own ``plugin.<leaf>.`` namespace, and a shared topic's
        ``subscribe_capability`` is a catalog capability or one this plugin
        declares."""
        if self.agent is None:
            return self
        prefix = f"plugin.{_plugin_leaf(self.id)}."
        declared: set[str] = set()
        for cap in self.agent.declared_capabilities:
            name = cap.id.removeprefix(prefix)
            if name == cap.id or not DECLARED_CAP_NAME_PATTERN.fullmatch(name):
                raise ManifestError(
                    f"declared capability {cap.id!r} must be {prefix}<name> with "
                    f"<name> matching {DECLARED_CAP_NAME_PATTERN.pattern}"
                )
            declared.add(cap.id)
        topics = [t.topic for t in self.agent.contributes.shared_topics]
        if len(set(topics)) != len(topics):
            raise ManifestError("agent.contributes.shared_topics topics must be unique")
        for shared in self.agent.contributes.shared_topics:
            if not shared.topic.startswith(prefix):
                raise ManifestError(
                    f"shared topic {shared.topic!r} must start with {prefix!r}"
                )
            cap = shared.subscribe_capability
            if not is_known_agent_capability(cap) and cap not in declared:
                raise ManifestError(
                    f"shared topic {shared.topic!r} subscribe_capability {cap!r} is "
                    "neither a known agent capability nor one of this plugin's "
                    "declared_capabilities"
                )
        return self

    @classmethod
    def from_yaml_text(cls, text: str) -> PluginManifest:
        try:
            data = yaml.safe_load(text)
        except yaml.YAMLError as exc:
            raise ManifestError(f"manifest is not valid YAML: {exc}") from exc
        if not isinstance(data, dict):
            raise ManifestError("manifest top-level must be a mapping")
        try:
            return cls.model_validate(data)
        except Exception as exc:
            raise ManifestError(str(exc)) from exc

    @classmethod
    def from_yaml_file(cls, path: str | Path) -> PluginManifest:
        p = Path(path)
        try:
            text = p.read_text(encoding="utf-8")
        except OSError as exc:
            raise ManifestError(f"cannot read manifest at {path}: {exc}") from exc
        return cls.from_yaml_text(text)

    def declared_permissions(self) -> set[str]:
        """Flat set of declared permission ids across both halves.

        Useful for the install dialog's permission preview where both
        agent and GCS capabilities render side by side. For the agent's
        own validation gate use :meth:`declared_agent_permissions`
        instead so GCS-only ids never trigger the agent's capability
        allowlist.
        """
        ids: set[str] = set()
        if self.agent is not None:
            ids.update(p.id for p in self.agent.permissions)
        if self.gcs is not None:
            ids.update(p.id for p in self.gcs.permissions)
        return ids

    def declared_agent_permissions(self) -> set[str]:
        """Set of permission ids the plugin requests from the agent.

        The agent enforces this list against its own capability catalog.
        GCS-only ids live under ``self.gcs.permissions`` and are policed
        by the browser-side runtime, not the agent.
        """
        if self.agent is None:
            return set()
        return {p.id for p in self.agent.permissions}


def schema_dict() -> dict[str, Any]:
    """Return the JSON Schema for :class:`PluginManifest`. Used by the SDK
    type generator and the public docs."""
    return PluginManifest.model_json_schema()


__all__ = [
    "ARCH_OS_PATTERN",
    "AgentBlock",
    "AgentContributes",
    "AgentPage",
    "BIN_NAME_PATTERN",
    "BIN_PREFIX",
    "CONTRIBUTION_ID_PATTERN",
    "Compatibility",
    "DECLARED_CAP_NAME_PATTERN",
    "DeclaredCapability",
    "FcParameter",
    "GcsBlock",
    "GcsContributes",
    "HardwareRequirements",
    "MavlinkComponent",
    "NodeSurface",
    "PLUGIN_ID_PATTERN",
    "PROFILES",
    "PayloadSpec",
    "PermissionRef",
    "PluginManifest",
    "Profile",
    "RequiredFcParameters",
    "ResourceImpact",
    "ResourceLimits",
    "SEMVER_PATTERN",
    "SHA256_HEX_PATTERN",
    "SHARED_TOPIC_PATTERN",
    "STANDARD_RESOURCE_CEILINGS",
    "Screenshot",
    "ServiceSpec",
    "SharedTopic",
    "VendorAttribution",
    "VisionContribution",
    "VisionModelRef",
    "log",
    "schema_dict",
]
