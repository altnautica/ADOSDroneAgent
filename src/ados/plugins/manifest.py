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
from typing import Annotated, Any, Literal, get_args

import yaml
from pydantic import BaseModel, ConfigDict, Field, field_validator, model_validator

from ados.core.logging import get_logger
from ados.plugins.capabilities import (
    is_known_agent_capability,
    is_known_gcs_capability,
)
from ados.plugins.errors import ManifestError
from ados.plugins.ready_check import parse_ready_check

log = get_logger("plugins.manifest")

PLUGIN_ID_PATTERN = re.compile(r"^[a-z0-9]+(\.[a-z0-9-]+)+$")
"""Reverse-DNS plugin ids: at least two dotted segments, lowercase plus digits
plus hyphen-only-after-first-char inside segments."""

SEMVER_PATTERN = re.compile(
    r"^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)"
    r"(?:-((?:0|[1-9]\d*|\d*[a-zA-Z-][0-9a-zA-Z-]*)(?:\.(?:0|[1-9]\d*|\d*[a-zA-Z-][0-9a-zA-Z-]*))*))?"
    r"(?:\+([0-9a-zA-Z-]+(?:\.[0-9a-zA-Z-]+)*))?$"
)

Profile = Literal["drone", "ground-station", "workstation", "compute"]

PROFILES: tuple[str, ...] = get_args(Profile)
"""Node profiles a plugin can target. Mirrors ``ados_config::node_profile()``."""

_PROFILE_ALIASES = {"ground_station": "ground-station"}
"""Underscore spellings accepted on input and rewritten to the canonical id."""

BIN_NAME_PATTERN = re.compile(r"^[a-z0-9][a-z0-9_-]{0,63}$")
"""Name of a packaged binary, referenced as ``bin:<name>``."""

ARCH_OS_PATTERN = re.compile(r"^[a-z0-9_]+-[a-z0-9_]+$")
"""``<arch>-<os>`` build key, e.g. ``aarch64-linux`` or ``aarch64-macos``."""

DECLARED_CAP_NAME_PATTERN = re.compile(r"^[a-z0-9][a-z0-9._-]{0,63}$")
"""Name segment of a plugin-declared capability ``plugin.<leaf>.<name>``."""

SHARED_TOPIC_PATTERN = re.compile(r"^plugin\.[a-z0-9-]+\.[a-z0-9._-]+$")
"""A topic a plugin publishes for other plugins: ``plugin.<leaf>.<rest>``."""

CONTRIBUTION_ID_PATTERN = re.compile(r"^[a-z0-9][a-z0-9_-]{0,63}$")
"""Id of a GCS agent page or node surface contribution."""

SHA256_HEX_PATTERN = re.compile(r"^[0-9a-f]{64}$")

BIN_PREFIX = "bin:"

STANDARD_RESOURCE_CEILINGS: dict[str, int] = {
    "max_ram_mb": 4096,
    "max_cpu_percent": 100,
    "max_pids": 256,
}
"""Per-field ceilings for the ``standard`` resource class. The ``heavy``
class is bounded only by the field limits on :class:`ResourceLimits`."""


def _plugin_leaf(plugin_id: str) -> str:
    """Last dot-segment of a plugin id (``com.example.world-engine`` ->
    ``world-engine``); the namespace for its declared capabilities and topics."""
    return plugin_id.rsplit(".", 1)[-1]


def _normalize_profiles(raw: Any) -> Any:
    """Rewrite accepted profile spellings to the canonical id before the
    ``Literal`` check runs."""
    if not isinstance(raw, list):
        return raw
    return [_PROFILE_ALIASES.get(p, p) if isinstance(p, str) else p for p in raw]


def _require_non_empty_profiles(value: list[str] | None, field: str) -> None:
    if value is not None and not value:
        raise ManifestError(f"{field} must list at least one profile when present")


def _bin_reference(token: str, field: str) -> str | None:
    """The ``<name>`` of a ``bin:<name>`` token, or ``None`` when ``token`` is
    not a packaged-binary reference. Refuses a malformed name."""
    if not token.startswith(BIN_PREFIX):
        return None
    name = token[len(BIN_PREFIX) :]
    if not BIN_NAME_PATTERN.fullmatch(name):
        raise ManifestError(
            f"{field} {token!r}: binary name must match {BIN_NAME_PATTERN.pattern}"
        )
    return name


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
    """Hard resource envelope for the plugin's processes.

    ``class`` picks the ceiling set: ``standard`` caps each field at
    :data:`STANDARD_RESOURCE_CEILINGS`; ``heavy`` lifts the caps to the field
    bounds below for plugins that run large models or simulations."""

    model_config = ConfigDict(validate_by_name=True, serialize_by_alias=True)

    resource_class: Literal["standard", "heavy"] = Field("standard", alias="class")
    max_ram_mb: int = Field(96, ge=8, le=65536)
    max_cpu_percent: int = Field(25, ge=1, le=3200)
    max_pids: int = Field(12, ge=1, le=4096)

    @model_validator(mode="after")
    def _enforce_class_ceilings(self) -> ResourceLimits:
        if self.resource_class == "heavy":
            return self
        for field, ceiling in STANDARD_RESOURCE_CEILINGS.items():
            value = getattr(self, field)
            if value > ceiling:
                raise ManifestError(
                    f"agent.resources.{field}={value} exceeds the standard class "
                    f"ceiling {ceiling}; set resources.class: heavy to raise it"
                )
        return self


class MavlinkComponent(_StrictModel):
    component_id: int = Field(..., ge=0, le=255)
    component_kind: Literal[
        "camera", "gimbal", "payload", "peripheral", "generic", "vio"
    ]
    sub_id: int | None = Field(None, ge=0, le=255)


class VendorAttribution(BaseModel):
    """Source-offer record for plugins that ship a vendor binary.

    Required when ``agent.contains_vendor_binary`` is true so the install
    dialog can surface the upstream repo, version, license, and a
    reachable source URL. Satisfies GPL-3.0 section 6 for plugins that
    distribute pre-compiled binaries built from GPL-compatible
    upstreams.

    Accepts two equivalent field name sets so existing first-party
    plugins do not have to migrate their manifest copy:

    * ``name`` / ``source_url`` / ``upstream_version`` (informational)
    * ``upstream_repo`` / ``source_offer_url`` / ``commit_sha`` (legalistic)

    Only ``license`` is strictly required; the other fields are
    optional and at least one of ``source_url`` or ``source_offer_url``
    plus one of ``upstream_version`` or ``commit_sha`` should be set
    for the GCS install dialog to render meaningful disclosure.
    """

    model_config = ConfigDict(extra="allow", str_strip_whitespace=True)

    name: str | None = None
    license: str = Field(..., min_length=1)
    source_url: str | None = None
    source_offer_url: str | None = None
    upstream_repo: str | None = None
    upstream_version: str | None = None
    commit_sha: str | None = None
    notice: str | None = None


class VisionModelRef(BaseModel):
    """A typed view of a ``vision.models[]`` entry. A model is delivered either BY
    REFERENCE — ``source`` + a pinned ``sha256``, per-board ``board_match`` — so the
    agent fetches/verifies/caches it (the model-delivery framework), or BUNDLED in the
    archive at ``path``. Lenient (``extra=ignore``) and additive: the manifest field stays
    free-form ``list[dict]`` for backward compatibility; this only parses the entries the
    framework resolves. The pinned ``sha256`` is signed (the manifest is signed), so a
    by-reference model is tamper-proof even though the weights are not inside the archive.
    """

    model_config = ConfigDict(extra="ignore", str_strip_whitespace=True)

    id: str
    runtime: str = "onnx"            # onnx | rknn | tensorrt | tflite | pytorch
    board_match: str = "generic"     # board family this variant targets (e.g. rk3588, orin, generic)
    sha256: str | None = None        # pinned hex digest the fetched model is verified against
    source: str | None = None        # where to fetch (registry ref / url); None ⇒ bundled or cache-only
    path: str | None = None          # relative in-archive path when the model is bundled


class VisionContribution(_StrictModel):
    behaviors: list[dict[str, Any]] = Field(default_factory=list)
    models: list[dict[str, Any]] = Field(default_factory=list)
    detectors: list[dict[str, Any]] = Field(default_factory=list)

    def model_refs(self) -> list[VisionModelRef]:
        """Typed view of the model entries that declare an id + runtime (additive, lenient)."""
        return [
            VisionModelRef.model_validate(d)
            for d in self.models
            if isinstance(d, dict) and d.get("id") and d.get("runtime")
        ]


class ServiceSpec(_StrictModel):
    """A long-running service a plugin declares on top of its main half.

    The supervisor renders one extra systemd unit per spec under the
    plugin slice and starts/stops it across the plugin's enable/disable
    lifecycle. Each spec reports readiness on the heartbeat so the GCS
    can show whether the declared daemon is actually up and serving.

    Fields:

    * ``name`` — short identifier, unique within the plugin. Used to
      build the unit name (``ados-plugin-<id>-<name>.service``, with
      dots and underscores sanitized to hyphens) and to key the
      readiness entry. Lowercase alnum plus ``.``, ``_``, ``-``.
    * ``command`` — the exec line the unit runs, as an argv (POSIX quoting,
      never a shell). The renderer re-quotes each word for systemd and
      refuses control characters and systemd exec prefixes; the plugin
      author is responsible for an absolute path or a binary on ``PATH``.
      A first word of ``bin:<name>`` runs the packaged binary ``<name>``
      from ``agent.binaries``.
    * ``ready_check`` — how readiness is probed. ``None`` ⇒ the service
      is ready iff its unit is active. An ``http(s)://127.0.0.1:<port>``
      URL ⇒ an HTTP GET, ready on a 2xx status. Any other value ⇒ an
      argv (POSIX quoting, never a shell) run as the plugin user inside
      the plugin's sandbox, ready on exit code 0.
    * ``restart`` — systemd restart policy for the unit.
    * ``profiles`` — node profiles the service runs on. ``None`` ⇒ every
      profile the plugin targets; an empty list is refused.
    * ``listen_ports`` — TCP/UDP ports the service binds (1024-65535, at
      most four, unique). Declaring any requires the ``network.listen`` and
      ``network.outbound`` capabilities.
    * ``slice`` — cgroup slice the unit runs in. Always the shared plugin
      slice; any other value is refused, so a plugin cannot move its own
      service out of the plugin resource envelope.

    Backward-compatible: a bare string element in ``services`` is
    coerced to ``{"name": <s>, "command": <s>}`` so existing
    ``services: ["foo"]`` manifests still parse.
    """

    name: str = Field(..., min_length=1, max_length=64)
    command: str = Field(..., min_length=1)
    ready_check: str | None = None
    restart: Literal["always", "on-failure", "no"] = "on-failure"
    slice: str = "ados-plugins.slice"
    profiles: list[Profile] | None = None
    listen_ports: list[Annotated[int, Field(ge=1024, le=65535)]] = Field(
        default_factory=list, max_length=4
    )

    @field_validator("name")
    @classmethod
    def _validate_name(cls, v: str) -> str:
        if not re.match(r"^[a-z0-9][a-z0-9._-]*$", v):
            raise ManifestError(
                f"service name {v!r} must be lowercase alnum plus ._- , "
                "starting with an alnum"
            )
        return v

    @field_validator("command")
    @classmethod
    def _validate_command(cls, v: str) -> str:
        _bin_reference(v.split()[0], "service command")
        return v

    @field_validator("profiles", mode="before")
    @classmethod
    def _canonical_profiles(cls, raw: Any) -> Any:
        return _normalize_profiles(raw)

    @field_validator("profiles")
    @classmethod
    def _validate_profiles(cls, v: list[Profile] | None) -> list[Profile] | None:
        _require_non_empty_profiles(v, "service profiles")
        return v

    @field_validator("listen_ports")
    @classmethod
    def _validate_listen_ports(cls, v: list[int]) -> list[int]:
        if len(set(v)) != len(v):
            raise ManifestError(f"service listen_ports {v} contains a duplicate port")
        return v

    @field_validator("slice")
    @classmethod
    def _validate_slice(cls, v: str) -> str:
        if v != "ados-plugins.slice":
            raise ManifestError(
                f"service slice {v!r} is not allowed; plugin services run in "
                "ados-plugins.slice"
            )
        return v

    @field_validator("ready_check")
    @classmethod
    def _validate_ready_check(cls, v: str | None) -> str | None:
        if v is None:
            return v
        try:
            parse_ready_check(v)
        except ValueError as exc:
            raise ManifestError(f"service ready_check {v!r} is invalid: {exc}") from exc
        return v

    def bin_name(self) -> str | None:
        """The packaged binary the command runs (``bin:<name>``), if any."""
        return _bin_reference(self.command.split()[0], "service command")


class SharedTopic(_StrictModel):
    """A topic the plugin publishes for other plugins to subscribe to.

    ``topic`` lives in the plugin's own namespace (``plugin.<leaf>.<rest>``);
    a subscriber needs ``subscribe_capability``, which is either a catalog
    capability or one of this plugin's ``declared_capabilities``."""

    topic: str = Field(..., min_length=1, max_length=128)
    subscribe_capability: str = Field(..., min_length=1)

    @field_validator("topic")
    @classmethod
    def _validate_topic(cls, v: str) -> str:
        if not SHARED_TOPIC_PATTERN.fullmatch(v):
            raise ManifestError(
                f"shared topic {v!r} must match plugin.<leaf>.<name> over [a-z0-9._-]"
            )
        return v


class AgentContributes(_StrictModel):
    services: list[ServiceSpec] = Field(default_factory=list)
    drivers: list[dict[str, Any]] = Field(default_factory=list)
    vision: VisionContribution | None = None
    # MCP tool / resource / prompt contributions (schema v3). Free-form and
    # additive; the plugin host reads them to build the tool registry and to
    # route a host->plugin tool.invoke. The agent does not interpret their
    # bodies beyond name + inputSchema + safety_class. A v2 manifest with no
    # tools block loads exactly as before, so the platform stays inert for
    # every existing plugin. Exposure needs the mcp.expose capability.
    tools: list[dict[str, Any]] = Field(default_factory=list)
    resources: list[dict[str, Any]] = Field(default_factory=list)
    prompts: list[dict[str, Any]] = Field(default_factory=list)
    shared_topics: list[SharedTopic] = Field(default_factory=list)

    @field_validator("services", mode="before")
    @classmethod
    def _coerce_services(cls, raw: Any) -> Any:
        """Accept the legacy ``list[str]`` shape alongside the rich
        ``list[ServiceSpec]`` shape. A bare string ``s`` becomes
        ``{"name": s, "command": s}`` so old manifests keep parsing."""
        if not isinstance(raw, list):
            return raw
        out: list[Any] = []
        for item in raw:
            if isinstance(item, str):
                out.append({"name": item, "command": item})
            else:
                out.append(item)
        return out

    def service_specs(self) -> list[ServiceSpec]:
        """Typed accessor for the declared services (already parsed)."""
        return list(self.services)


_ENTRYPOINT_PATH = re.compile(r"[A-Za-z0-9._/-]+")
_IDENTIFIER = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")


def _is_relative_posix_path(value: str) -> bool:
    """A relative posix path over ``[A-Za-z0-9._/-]`` with no empty segment
    and no segment starting with ``..``."""
    return bool(_ENTRYPOINT_PATH.fullmatch(value)) and all(
        part and not part.startswith("..") for part in value.split("/")
    )


def _validate_relative_path(value: str, field: str) -> str:
    if not _is_relative_posix_path(value):
        raise ManifestError(
            f"{field} {value!r} must be a relative posix path over "
            "[A-Za-z0-9._/-] with no empty or '..' segment"
        )
    return value


def _validate_entrypoint(value: str) -> str:
    """Accept exactly what the Rust manifest parser accepts.

    Either ``module.path:Class`` (every segment a Python identifier), or a
    relative posix path over ``[A-Za-z0-9._/-]`` with no empty segment and no
    segment starting with ``..``. The value is interpolated into a generated
    unit's ``ExecStart=`` for a rust plugin, so a space, ``%`` or ``$`` would
    add argv words or expand as a systemd specifier; and the two lifecycle
    paths (LAN and cloud relay) must refuse the same manifests.
    """
    module, sep, klass = value.partition(":")
    if sep:
        ok = bool(_IDENTIFIER.fullmatch(klass)) and all(
            _IDENTIFIER.fullmatch(part) for part in module.split(".")
        )
    else:
        ok = _is_relative_posix_path(value)
    if not ok:
        raise ManifestError(
            f"entrypoint {value!r} must be a relative posix path over "
            "[A-Za-z0-9._/-] with no empty or '..' segment, or a module:Class "
            "reference"
        )
    return value


class PayloadSpec(_StrictModel):
    """A large file fetched at install time instead of shipped in the archive
    (model weights, maps). The installer downloads ``source``, checks the
    size and SHA-256, and places it at ``path`` under the plugin directory.
    ``profiles`` / ``arch_os`` narrow which nodes fetch it; ``None`` means
    every node the plugin installs on."""

    path: str = Field(..., min_length=1)
    source: str = Field(..., min_length=1)
    sha256: str
    size_bytes: int = Field(..., ge=1, le=1_073_741_824)
    profiles: list[Profile] | None = None
    arch_os: str | None = None

    @field_validator("path")
    @classmethod
    def _validate_path(cls, v: str) -> str:
        return _validate_relative_path(v, "payload path")

    @field_validator("source")
    @classmethod
    def _validate_source(cls, v: str) -> str:
        if not v.startswith("https://"):
            raise ManifestError(f"payload source must use https://, got {v!r}")
        return v

    @field_validator("sha256")
    @classmethod
    def _validate_sha256(cls, v: str) -> str:
        if not SHA256_HEX_PATTERN.fullmatch(v):
            raise ManifestError(
                f"payload sha256 {v!r} must be 64 lowercase hex characters"
            )
        return v

    @field_validator("profiles", mode="before")
    @classmethod
    def _canonical_profiles(cls, raw: Any) -> Any:
        return _normalize_profiles(raw)

    @field_validator("arch_os")
    @classmethod
    def _validate_arch_os(cls, v: str | None) -> str | None:
        if v is not None and not ARCH_OS_PATTERN.fullmatch(v):
            raise ManifestError(f"payload arch_os {v!r} must look like aarch64-linux")
        return v


class DeclaredCapability(_StrictModel):
    """A capability the plugin defines for other plugins to request, e.g. to
    subscribe to one of its shared topics. The id is namespaced to the
    plugin: ``plugin.<leaf>.<name>`` (checked on :class:`PluginManifest`,
    which knows the plugin id)."""

    id: str = Field(..., min_length=1)
    risk: Literal["low", "medium", "high", "critical"]
    description: str = ""




class AgentBlock(_StrictModel):
    """Agent-half manifest block."""

    entrypoint: str
    """An entry-point id (``module:Class``) for built-in plugins, a relative
    path to a Python module inside the archive for third-party plugins, or
    ``bin:<name>`` naming a packaged binary in ``binaries`` (``rust``
    runtime only)."""

    isolation: Literal["subprocess", "inprocess"] = "subprocess"
    """Default subprocess. ``inprocess`` is allowed only for first-party
    built-in plugins; the supervisor enforces this."""

    runtime: Literal["python", "rust"] = "python"
    """Which executor systemd starts: the shared Python runner (default)
    or the plugin's own binary (``rust``). Additive and optional, so an
    older manifest with no ``runtime`` field loads as ``python``."""

    permissions: list[PermissionRef] = Field(default_factory=list)
    resources: ResourceLimits = Field(default_factory=ResourceLimits)
    contributes: AgentContributes = Field(default_factory=AgentContributes)
    mavlink_components: list[MavlinkComponent] = Field(default_factory=list)
    contains_vendor_binary: bool = False
    test_fixtures: dict[str, str] = Field(default_factory=dict)
    """Map of friendly name to fixture YAML path (relative to plugin root).
    Consumed by the SDK test harness so plugin tests can replay scenarios
    by name. Paths are validated for traversal at install time."""

    vendor_attribution: list[VendorAttribution] = Field(default_factory=list)
    """Source-offer records for vendor-binary plugins. Required (non-empty)
    when ``contains_vendor_binary`` is true; empty list for pure-Python
    plugins. Multiple entries permitted when a plugin bundles binaries
    from more than one upstream. Schema v2."""

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
    ``/var/ados/plugin-data/<plugin_id>/config/<agent_id>.yaml``.
    Default false preserves single-config behavior from the v1
    schema. Schema v2."""

    target_profiles: list[Profile] = Field(default_factory=lambda: ["drone"])
    """Node profiles the plugin is compatible with. Default ``["drone"]``
    so manifests that omit the field stay drone-only. A plugin that wants
    to surface on a ground station declares ``["ground-station"]``, on the
    operator workstation ``["workstation"]``, on a headless compute node
    ``["compute"]``; a multi-target plugin declares more than one.
    ``ground_station`` is accepted and rewritten to ``ground-station``."""

    binaries: dict[str, dict[str, str]] = Field(default_factory=dict)
    """Packaged binaries by name, each a map of ``<arch>-<os>`` to the
    archive-relative path of that build. Referenced as ``bin:<name>`` from
    ``entrypoint`` or the first word of a service ``command``."""

    payloads: list[PayloadSpec] = Field(default_factory=list)
    """Files fetched at install time and verified by SHA-256."""

    http: bool = False
    """True when the plugin serves an HTTP surface the host proxies."""

    declared_capabilities: list[DeclaredCapability] = Field(default_factory=list)
    """Capabilities this plugin defines (``plugin.<leaf>.<name>``) for other
    plugins to request."""

    @field_validator("target_profiles", mode="before")
    @classmethod
    def _canonical_profiles(cls, raw: Any) -> Any:
        return _normalize_profiles(raw)

    @field_validator("target_profiles")
    @classmethod
    def _validate_target_profiles(cls, value: list[Profile]) -> list[Profile]:
        if not value:
            raise ManifestError(
                "agent.target_profiles must list at least one profile",
            )
        # Dedupe preserving order so the wire shape stays deterministic
        # across reads. Pydantic's Literal validation runs before this
        # hook, so each entry is already a known profile string.
        return list(dict.fromkeys(value))

    @field_validator("entrypoint")
    @classmethod
    def _validate_entrypoint(cls, v: str) -> str:
        if _bin_reference(v, "agent.entrypoint") is not None:
            return v
        return _validate_entrypoint(v)

    @field_validator("binaries")
    @classmethod
    def _validate_binaries(
        cls, raw: dict[str, dict[str, str]]
    ) -> dict[str, dict[str, str]]:
        for name, builds in raw.items():
            if not BIN_NAME_PATTERN.fullmatch(name):
                raise ManifestError(
                    f"agent.binaries key {name!r} must match {BIN_NAME_PATTERN.pattern}"
                )
            for arch_os, path in builds.items():
                if not ARCH_OS_PATTERN.fullmatch(arch_os):
                    raise ManifestError(
                        f"agent.binaries[{name!r}] key {arch_os!r} must look like "
                        "aarch64-linux"
                    )
                _validate_relative_path(path, f"agent.binaries[{name!r}][{arch_os!r}]")
        return raw

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
    def _warn_unknown_capabilities(self) -> AgentBlock:
        """Log a warning for any permission id not in the canonical
        catalog. Older or experimental manifests must still load, so
        this never rejects; it only flags drift between the manifest
        author and the host's known capability set. ``plugin.*`` ids are
        capabilities other plugins declare, so they are outside the
        catalog by design and never flagged.
        """
        for perm in self.permissions:
            if perm.id.startswith("plugin."):
                continue
            if not is_known_agent_capability(perm.id):
                log.warning(
                    "plugin_manifest_unknown_agent_capability",
                    capability=perm.id,
                )
        return self

    @model_validator(mode="after")
    def _validate_vendor_attribution_pairing(self) -> AgentBlock:
        """``contains_vendor_binary`` and ``vendor_attribution`` must
        agree. A vendor-binary plugin without a source-offer record
        would ship a GPL-incompatible install dialog; a source-offer
        record without a declared vendor binary is a manifest typo
        that the operator would not see in the install dialog risk
        summary."""
        has_attribution = bool(self.vendor_attribution)
        if self.contains_vendor_binary and not has_attribution:
            raise ManifestError(
                "agent.contains_vendor_binary is true but "
                "agent.vendor_attribution is empty; at least one "
                "source-offer record is required for vendor-binary "
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
    def _validate_subprocess_spawn_capability(self) -> AgentBlock:
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

    @model_validator(mode="after")
    def _validate_runtime_isolation(self) -> AgentBlock:
        """A rust runtime always runs as its own binary under systemd, so
        there is no in-process Python analog. Reject the combination up
        front rather than letting it reach the supervisor."""
        if self.runtime == "rust" and self.isolation == "inprocess":
            raise ManifestError(
                "Rust plugins have no in-process analog; rust runtime "
                "requires subprocess isolation"
            )
        return self

    @model_validator(mode="after")
    def _validate_binary_references(self) -> AgentBlock:
        """Every ``bin:<name>`` (the entrypoint or a service command's
        first word) must name a key of ``binaries``, and a ``bin:``
        entrypoint only makes sense for the ``rust`` runtime."""
        entry_bin = _bin_reference(self.entrypoint, "agent.entrypoint")
        if entry_bin is not None and self.runtime != "rust":
            raise ManifestError(
                f"agent.entrypoint {self.entrypoint!r} is a packaged binary; "
                "set agent.runtime: rust"
            )
        referenced = [("agent.entrypoint", entry_bin)] + [
            (f"service {svc.name!r} command", svc.bin_name())
            for svc in self.contributes.services
        ]
        for where, name in referenced:
            if name is not None and name not in self.binaries:
                raise ManifestError(
                    f"{where} runs bin:{name} but agent.binaries has no {name!r} key"
                )
        return self

    @model_validator(mode="after")
    def _validate_unique_ids(self) -> AgentBlock:
        paths = [p.path for p in self.payloads]
        if len(set(paths)) != len(paths):
            raise ManifestError("agent.payloads paths must be unique")
        cap_ids = [c.id for c in self.declared_capabilities]
        if len(set(cap_ids)) != len(cap_ids):
            raise ManifestError("agent.declared_capabilities ids must be unique")
        return self

    @model_validator(mode="after")
    def _validate_listener_capabilities(self) -> AgentBlock:
        """A service that binds a port needs ``network.listen``, and also
        ``network.outbound``: only that grant opens the inet socket
        families a listener needs."""
        if not any(svc.listen_ports for svc in self.contributes.services):
            return self
        declared = {p.id for p in self.permissions}
        missing = [
            cap for cap in ("network.listen", "network.outbound") if cap not in declared
        ]
        if missing:
            raise ManifestError(
                "a service declares listen_ports but agent.permissions is missing "
                + ", ".join(missing)
            )
        return self


def _validate_contribution_id(value: str, field: str) -> str:
    if not CONTRIBUTION_ID_PATTERN.fullmatch(value):
        raise ManifestError(
            f"{field} {value!r} must match {CONTRIBUTION_ID_PATTERN.pattern}"
        )
    return value


class AgentPage(_StrictModel):
    """A full page the plugin adds to a node's navigation in the GCS.

    ``section`` / ``after`` / ``order`` place it in the menu; ``profile``
    narrows the node profiles it appears on (``None`` = every profile).
    ``setup_for`` marks this page as the setup flow of another page in the
    same list."""

    id: str
    title: str = Field(..., min_length=1, max_length=80)
    icon: str | None = None
    section: str | None = None
    after: str | None = None
    order: int | None = None
    profile: list[Profile] | None = None
    setup_for: str | None = None

    @field_validator("id")
    @classmethod
    def _validate_id(cls, v: str) -> str:
        return _validate_contribution_id(v, "agent page id")

    @field_validator("profile", mode="before")
    @classmethod
    def _canonical_profiles(cls, raw: Any) -> Any:
        return _normalize_profiles(raw)

    @field_validator("profile")
    @classmethod
    def _validate_profile(cls, v: list[Profile] | None) -> list[Profile] | None:
        _require_non_empty_profiles(v, "agent page profile")
        return v


class NodeSurface(_StrictModel):
    """A card the plugin adds to a node's overview in the GCS, shown on the
    listed node profiles, optionally under one of the fixed groups."""

    id: str
    title: str = Field(..., min_length=1, max_length=80)
    profile: list[Profile]
    group: Literal["status", "vehicle", "link", "device", "compute"] | None = None
    order: int | None = None

    @field_validator("id")
    @classmethod
    def _validate_id(cls, v: str) -> str:
        return _validate_contribution_id(v, "node surface id")

    @field_validator("profile", mode="before")
    @classmethod
    def _canonical_profiles(cls, raw: Any) -> Any:
        return _normalize_profiles(raw)

    @field_validator("profile")
    @classmethod
    def _validate_profile(cls, v: list[Profile]) -> list[Profile]:
        _require_non_empty_profiles(v, "node surface profile")
        return v


class GcsContributes(_StrictModel):
    panels: list[dict[str, Any]] = Field(default_factory=list)
    overlays: list[dict[str, Any]] = Field(default_factory=list)
    notifications: list[dict[str, Any]] = Field(default_factory=list)
    smart_functions: list[dict[str, Any]] = Field(default_factory=list)
    # Node-detail tab contributions (the node.detail.tab slot), each with its
    # profile narrowing plus title/icon/order. Free-form and additive; the GCS
    # contribution registry reads them, the agent does not interpret them.
    tabs: list[dict[str, Any]] = Field(default_factory=list)
    # Declarative parameter contributions. The GCS renders these as native
    # config controls; the agent reads the same per-drone keys live each loop.
    parameters: list[dict[str, Any]] = Field(default_factory=list)
    # Detection/vision models the plugin ships, offered by the model picker.
    models: list[dict[str, Any]] = Field(default_factory=list)
    # Flight Skill contributions. Each entry surfaces a behavior as a
    # first-class Skill in the cockpit Skill Bar (toggle, hotkey/gamepad
    # binding, activation via per-drone config, read-back via an event
    # topic). The agent does not interpret these; they are read by the
    # GCS skill registry. Kept free-form (``dict``) and additive so a
    # forward-compatible manifest parses without a schema bump.
    skills: list[dict[str, Any]] = Field(default_factory=list)
    # Target-action contributions. Each entry is an action a plugin offers for
    # a clicked detection in the cockpit target overlay: designate the target,
    # then flip a per-drone config key. Listed beside the built-in actions in
    # one popup. The agent does not interpret these; the GCS target-action
    # registry reads them. Free-form and additive like ``skills``.
    target_actions: list[dict[str, Any]] = Field(default_factory=list)
    # MCP tool / resource / prompt contributions for the GCS half (schema v3).
    # The GCS contribution registry reads these to assemble the MCP tools/list;
    # a GCS-only plugin's tools route through the GCS bridge (no agent socket).
    # Free-form and additive like ``skills``. Exposure needs the mcp.expose cap.
    tools: list[dict[str, Any]] = Field(default_factory=list)
    resources: list[dict[str, Any]] = Field(default_factory=list)
    prompts: list[dict[str, Any]] = Field(default_factory=list)
    # Plugin settings rendered by the GCS settings surface. Free-form; the
    # agent does not interpret them.
    settings: list[dict[str, Any]] = Field(default_factory=list)
    agent_pages: list[AgentPage] = Field(default_factory=list)
    node_surfaces: list[NodeSurface] = Field(default_factory=list)

    @model_validator(mode="after")
    def _validate_pages_and_surfaces(self) -> GcsContributes:
        """Page and surface ids are unique; ``setup_for`` names another page
        in the same list, and that page is not itself a setup page."""
        page_ids = [p.id for p in self.agent_pages]
        if len(set(page_ids)) != len(page_ids):
            raise ManifestError("gcs.contributes.agent_pages ids must be unique")
        surface_ids = [s.id for s in self.node_surfaces]
        if len(set(surface_ids)) != len(surface_ids):
            raise ManifestError("gcs.contributes.node_surfaces ids must be unique")
        by_id = {p.id: p for p in self.agent_pages}
        for page in self.agent_pages:
            if page.setup_for is None:
                continue
            if page.setup_for == page.id:
                raise ManifestError(
                    f"agent page {page.id!r} cannot be its own setup_for"
                )
            target = by_id.get(page.setup_for)
            if target is None:
                raise ManifestError(
                    f"agent page {page.id!r} setup_for {page.setup_for!r} names "
                    "no page in agent_pages"
                )
            if target.setup_for is not None:
                raise ManifestError(
                    f"agent page {page.id!r} setup_for {page.setup_for!r} names a "
                    "page that is itself a setup page"
                )
        return self


class GcsBlock(_StrictModel):
    """GCS-half manifest block."""

    entrypoint: str
    """Relative path inside the archive to the GCS bundle entrypoint
    (``gcs/plugin.bundle.js``)."""

    isolation: Literal["iframe", "inline"] = "iframe"
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

    @model_validator(mode="after")
    def _warn_unknown_capabilities(self) -> GcsBlock:
        """Log a warning for any GCS permission id not in the known GCS
        capability set. Older or experimental manifests must still load,
        so this never rejects; it only flags drift between the manifest
        author and the host's known GCS capability set. Symmetric with
        the agent-half validator.
        """
        for perm in self.permissions:
            if not is_known_gcs_capability(perm.id):
                log.warning(
                    "plugin_manifest_unknown_gcs_capability",
                    capability=perm.id,
                )
        return self


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
