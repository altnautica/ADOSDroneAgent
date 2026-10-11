"""Agent-half manifest block."""

from __future__ import annotations

from typing import Any, Literal

from pydantic import Field, field_validator, model_validator

from ados.plugins.capabilities import (
    is_known_agent_capability,
)
from ados.plugins.errors import ManifestError
from ados.plugins.manifest._agent_parts import (
    AgentContributes,
    DeclaredCapability,
    MavlinkComponent,
    PayloadSpec,
    ResourceLimits,
    VendorAttribution,
    _validate_entrypoint,
    _validate_relative_path,
)
from ados.plugins.manifest._common import (
    ARCH_OS_PATTERN,
    BIN_NAME_PATTERN,
    PermissionRef,
    Profile,
    _bin_reference,
    _normalize_permission,
    _normalize_profiles,
    _StrictModel,
    log,
)


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
