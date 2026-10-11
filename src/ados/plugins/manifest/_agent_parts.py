"""Agent-half manifest sub-models: resources, services, payloads, capabilities."""

from __future__ import annotations

import re
from typing import Annotated, Any, Literal

from pydantic import BaseModel, ConfigDict, Field, field_validator, model_validator

from ados.plugins.errors import ManifestError
from ados.plugins.manifest._common import (
    ARCH_OS_PATTERN,
    SHA256_HEX_PATTERN,
    SHARED_TOPIC_PATTERN,
    STANDARD_RESOURCE_CEILINGS,
    Profile,
    _bin_reference,
    _normalize_profiles,
    _require_non_empty_profiles,
    _StrictModel,
)
from ados.plugins.ready_check import parse_ready_check


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
    runtime: str = "onnx"            # onnx | rknn | hailo | tflite | pytorch
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
      build the unit name (``ados-plugin-<id-dashed>-<hash8>-<service>.service``,
      dots in the id and service sanitized to hyphens) and to key the
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
