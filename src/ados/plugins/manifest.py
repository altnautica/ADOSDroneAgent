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

import re
from pathlib import Path
from typing import Any, Literal

import yaml
from pydantic import BaseModel, ConfigDict, Field, field_validator, model_validator

from ados.core.logging import get_logger
from ados.plugins.capabilities import is_known_agent_capability
from ados.plugins.errors import ManifestError

log = get_logger("plugins.manifest")

PLUGIN_ID_PATTERN = re.compile(r"^[a-z0-9]+(\.[a-z0-9-]+)+$")
"""Reverse-DNS plugin ids: at least two dotted segments, lowercase plus digits
plus hyphen-only-after-first-char inside segments."""

SEMVER_PATTERN = re.compile(
    r"^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)"
    r"(?:-((?:0|[1-9]\d*|\d*[a-zA-Z-][0-9a-zA-Z-]*)(?:\.(?:0|[1-9]\d*|\d*[a-zA-Z-][0-9a-zA-Z-]*))*))?"
    r"(?:\+([0-9a-zA-Z-]+(?:\.[0-9a-zA-Z-]+)*))?$"
)


class _StrictModel(BaseModel):
    model_config = ConfigDict(extra="forbid", str_strip_whitespace=True)


class PermissionRef(_StrictModel):
    """Object form of a permission entry: ``{id, required, degraded_behavior}``."""

    id: str
    required: bool = True
    degraded_behavior: str | None = None


def _normalize_permission(value: Any) -> dict[str, Any]:
    """Canonicalize a permission entry to dict shape (string OR object)."""
    if isinstance(value, str):
        return {"id": value, "required": True, "degraded_behavior": None}
    if isinstance(value, dict):
        return value
    raise ManifestError(f"permission entry must be str or object, got {type(value)}")


class ResourceLimits(_StrictModel):
    max_ram_mb: int = Field(96, ge=8, le=4096)
    max_cpu_percent: int = Field(25, ge=1, le=100)
    max_pids: int = Field(12, ge=1, le=256)


class MavlinkComponent(_StrictModel):
    component_id: int = Field(..., ge=0, le=255)
    component_kind: Literal[
        "camera", "gimbal", "payload", "peripheral", "generic", "vio"
    ]
    sub_id: int | None = Field(None, ge=0, le=255)


class VendorAttribution(_StrictModel):
    """Source-offer record for plugins that ship a vendor binary.

    Required when ``agent.contains_vendor_binary`` is true so the
    install dialog can surface the upstream repo, commit, license, and
    a reachable source-offer URL. Satisfies GPL-3.0 section 6 for
    plugins that distribute pre-compiled binaries built from
    GPL-compatible upstreams.
    """

    upstream_repo: str = Field(..., min_length=1)
    commit_sha: str = Field(..., min_length=1)
    license: str = Field(..., min_length=1)
    source_offer_url: str = Field(..., min_length=1)


class VisionContribution(_StrictModel):
    behaviors: list[dict[str, Any]] = Field(default_factory=list)
    models: list[dict[str, Any]] = Field(default_factory=list)
    detectors: list[dict[str, Any]] = Field(default_factory=list)


class AgentContributes(_StrictModel):
    services: list[str] = Field(default_factory=list)
    drivers: list[dict[str, Any]] = Field(default_factory=list)
    vision: VisionContribution | None = None


def _validate_entrypoint(value: str) -> str:
    """Reject path-traversal or absolute paths in entrypoint fields.

    Module-id form (``module:Class``) passes through. Path form must
    be relative and contain no ``..`` segments.
    """
    if ":" in value:
        return value
    if value.startswith("/") or value.startswith("\\") or "\\" in value:
        raise ManifestError(f"entrypoint must be a relative posix path, got {value!r}")
    parts = value.split("/")
    if any(p == ".." or p.startswith("..") for p in parts):
        raise ManifestError(f"entrypoint must not contain .. segments, got {value!r}")
    if not value.strip():
        raise ManifestError("entrypoint must not be empty")
    return value


class AgentBlock(_StrictModel):
    """Agent-half manifest block."""

    entrypoint: str
    """Either an entry-point id (``module:Class``) for built-in plugins,
    or a relative path to a Python module inside the archive for
    third-party plugins."""

    isolation: Literal["subprocess", "inprocess"] = "subprocess"
    """Default subprocess. ``inprocess`` is allowed only for first-party
    built-in plugins; the supervisor enforces this."""

    permissions: list[PermissionRef] = Field(default_factory=list)
    resources: ResourceLimits = Field(default_factory=ResourceLimits)
    contributes: AgentContributes = Field(default_factory=AgentContributes)
    mavlink_components: list[MavlinkComponent] = Field(default_factory=list)
    contains_vendor_binary: bool = False
    test_fixtures: dict[str, str] = Field(default_factory=dict)
    """Map of friendly name to fixture YAML path (relative to plugin root).
    Consumed by the SDK test harness so plugin tests can replay scenarios
    by name. Paths are validated for traversal at install time."""

    vendor_attribution: VendorAttribution | None = None
    """Source-offer record for vendor-binary plugins. Required when
    ``contains_vendor_binary`` is true; absent for pure-Python
    plugins. Schema v2."""

    subprocess_spawn: list[str] | None = None
    """Allowlist of binary basenames the plugin may exec via
    ``ctx.process.spawn``. Paths resolve relative to the plugin's
    ``data_dir``. Implies the ``process.spawn`` capability must be
    declared in ``permissions``. Empty or absent means the plugin
    cannot spawn any subprocess. Schema v2."""

    per_drone_config: bool = False
    """When true, the supervisor runs one process instance per
    connected drone with a distinct ``ctx.agent_id`` and a per-drone
    config dict at
    ``/var/lib/ados/plugins/<plugin_id>/config/<agent_id>.yaml``.
    Default false preserves single-config behavior from the v1
    schema. Schema v2."""

    target_profiles: list[Literal["drone", "ground-station", "lite"]] = Field(
        default_factory=lambda: ["drone"],
    )
    """Node profiles the plugin is compatible with. Default ``["drone"]``
    so existing manifests that omit the field stay drone-only — which
    matches the only first-party plugin that exists today
    (``com.altnautica.vision-nav``). A plugin that wants to surface on a
    ground station declares ``["ground-station"]``; a multi-target
    plugin declares both. Schema v2 — older manifests get the default."""

    @field_validator("target_profiles")
    @classmethod
    def _validate_target_profiles(
        cls, value: list[str]
    ) -> list[Literal["drone", "ground-station", "lite"]]:
        if not value:
            raise ManifestError(
                "agent.target_profiles must list at least one profile",
            )
        # Dedupe preserving order so the wire shape stays deterministic
        # across reads. Pydantic's Literal validation runs before this
        # hook, so each entry is already a known profile string.
        seen: set[str] = set()
        deduped: list[Literal["drone", "ground-station", "lite"]] = []
        for entry in value:
            if entry in seen:
                continue
            seen.add(entry)
            deduped.append(entry)  # type: ignore[arg-type]
        return deduped

    @field_validator("entrypoint")
    @classmethod
    def _validate_entrypoint(cls, v: str) -> str:
        return _validate_entrypoint(v)

    @field_validator("test_fixtures")
    @classmethod
    def _validate_test_fixtures(cls, raw: dict[str, str]) -> dict[str, str]:
        for name, path in raw.items():
            if not isinstance(name, str) or not name:
                raise ManifestError(
                    f"test_fixtures key must be a non-empty string, got {name!r}"
                )
            if not isinstance(path, str) or not path:
                raise ManifestError(
                    f"test_fixtures[{name!r}] must be a non-empty path"
                )
            _validate_entrypoint(path)
        return raw

    @field_validator("permissions", mode="before")
    @classmethod
    def _normalize_perms(cls, raw: Any) -> Any:
        if not isinstance(raw, list):
            return raw
        return [_normalize_permission(item) for item in raw]

    @model_validator(mode="after")
    def _warn_unknown_capabilities(self) -> "AgentBlock":
        """Log a warning for any permission id not in the canonical
        catalog. Older or experimental manifests must still load, so
        this never rejects; it only flags drift between the manifest
        author and the host's known capability set.
        """
        for perm in self.permissions:
            if not is_known_agent_capability(perm.id):
                log.warning(
                    "plugin_manifest_unknown_agent_capability",
                    capability=perm.id,
                )
        return self

    @model_validator(mode="after")
    def _validate_vendor_attribution_pairing(self) -> "AgentBlock":
        """``contains_vendor_binary`` and ``vendor_attribution`` must
        agree. A vendor-binary plugin without a source-offer record
        would ship a GPL-incompatible install dialog; a source-offer
        record without a declared vendor binary is a manifest typo
        that the operator would not see in the install dialog risk
        summary."""
        has_attribution = self.vendor_attribution is not None
        if self.contains_vendor_binary and not has_attribution:
            raise ManifestError(
                "agent.contains_vendor_binary is true but "
                "agent.vendor_attribution is missing; the four "
                "source-offer fields are required for vendor-binary "
                "plugins"
            )
        if has_attribution and not self.contains_vendor_binary:
            raise ManifestError(
                "agent.vendor_attribution is set but "
                "agent.contains_vendor_binary is false; set the flag "
                "or remove the attribution block"
            )
        return self

    @model_validator(mode="after")
    def _validate_subprocess_spawn_capability(self) -> "AgentBlock":
        """If ``subprocess_spawn`` lists any binary, the plugin must
        declare the ``process.spawn`` capability so the operator sees
        the Critical-tier risk badge at install time. The supervisor
        also enforces the allowlist at spawn time, but the manifest
        validator surfaces the missing declaration up-front with an
        actionable error."""
        spawns = self.subprocess_spawn or []
        if not spawns:
            return self
        declared = {p.id for p in self.permissions}
        if "process.spawn" not in declared:
            raise ManifestError(
                "agent.subprocess_spawn lists "
                f"{len(spawns)} binary path(s) but the "
                "process.spawn capability is not declared in "
                "agent.permissions; add it so the operator can "
                "review the spawn allowlist at install time"
            )
        return self


class GcsContributes(_StrictModel):
    panels: list[dict[str, Any]] = Field(default_factory=list)
    overlays: list[dict[str, Any]] = Field(default_factory=list)
    notifications: list[dict[str, Any]] = Field(default_factory=list)
    smart_functions: list[dict[str, Any]] = Field(default_factory=list)


class GcsBlock(_StrictModel):
    """GCS-half manifest block."""

    entrypoint: str
    """Relative path inside the archive to the GCS bundle entrypoint
    (``gcs/dist/index.js``)."""

    isolation: Literal["iframe", "worker", "inline"] = "iframe"
    """Inline is restricted to first-party signers."""

    permissions: list[PermissionRef] = Field(default_factory=list)
    contributes: GcsContributes = Field(default_factory=GcsContributes)
    locales: list[str] = Field(default_factory=list)

    @field_validator("entrypoint")
    @classmethod
    def _validate_entrypoint(cls, v: str) -> str:
        return _validate_entrypoint(v)

    @field_validator("permissions", mode="before")
    @classmethod
    def _normalize_perms(cls, raw: Any) -> Any:
        if not isinstance(raw, list):
            return raw
        return [_normalize_permission(item) for item in raw]


class Compatibility(_StrictModel):
    ados_version: str = Field(..., min_length=1)
    """Semver range, e.g. ``>=0.9.0,<1.0.0``."""

    gcs_version: str | None = None
    supported_boards: list[str] = Field(default_factory=list)


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

    schema_version: int = Field(1, ge=1, le=2)
    """Manifest schema version. ``1`` is the original baseline. ``2``
    unlocks the additional ``agent`` fields ``vendor_attribution``,
    ``subprocess_spawn``, and ``per_drone_config``. Both versions
    parse identical-shape v1 manifests; the version field is
    informational so older tooling can route on schema generation."""
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
    def _at_least_one_half(self) -> "PluginManifest":
        if self.agent is None and self.gcs is None:
            raise ManifestError(
                f"plugin {self.id} declares neither agent nor gcs half; "
                "at least one is required"
            )
        return self

    @classmethod
    def from_yaml_text(cls, text: str) -> "PluginManifest":
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
    def from_yaml_file(cls, path: str | Path) -> "PluginManifest":
        p = Path(path)
        try:
            text = p.read_text(encoding="utf-8")
        except OSError as exc:
            raise ManifestError(f"cannot read manifest at {path}: {exc}") from exc
        return cls.from_yaml_text(text)

    def declared_permissions(self) -> set[str]:
        """Flat set of declared permission ids across both halves."""
        ids: set[str] = set()
        if self.agent is not None:
            ids.update(p.id for p in self.agent.permissions)
        if self.gcs is not None:
            ids.update(p.id for p in self.gcs.permissions)
        return ids


def schema_dict() -> dict[str, Any]:
    """Return the JSON Schema for :class:`PluginManifest`. Used by the SDK
    type generator and the public docs."""
    return PluginManifest.model_json_schema()
