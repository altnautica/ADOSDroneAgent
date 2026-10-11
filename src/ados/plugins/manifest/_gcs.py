"""GCS-half manifest block and its contributions."""

from __future__ import annotations

from typing import Any, Literal

from pydantic import Field, field_validator, model_validator

from ados.plugins.capabilities import is_known_gcs_capability
from ados.plugins.errors import ManifestError
from ados.plugins.manifest._agent_parts import _validate_entrypoint
from ados.plugins.manifest._common import (
    CONTRIBUTION_ID_PATTERN,
    PermissionRef,
    Profile,
    _normalize_permission,
    _normalize_profiles,
    _require_non_empty_profiles,
    _StrictModel,
    log,
)


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
    # Map-overlay and mission-template contributions, read by the GCS map and
    # mission-template picker. Free-form; the agent does not interpret them.
    map_overlays: list[dict[str, Any]] = Field(default_factory=list)
    mission_templates: list[dict[str, Any]] = Field(default_factory=list)
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


# The GCS contribution lists rendered from the half's bundle (an iframe or an
# inline module); declaring any of them requires ``gcs.entrypoint``.
_GCS_BUNDLE_CONTRIBUTIONS: tuple[str, ...] = (
    "panels",
    "tabs",
    "agent_pages",
    "node_surfaces",
)


class GcsBlock(_StrictModel):
    """GCS-half manifest block."""

    entrypoint: str | None = None
    """Relative path inside the archive to the GCS bundle entrypoint
    (``gcs/plugin.bundle.js``). May be omitted only when the half contributes
    nothing rendered from a bundle (no ``panels``, ``tabs``, ``agent_pages``
    or ``node_surfaces``), so a parameters- or skills-only half needs no
    bundle."""

    isolation: Literal["iframe", "inline"] = "iframe"
    """Inline is restricted to first-party signers."""

    permissions: list[PermissionRef] = Field(default_factory=list)
    contributes: GcsContributes = Field(default_factory=GcsContributes)
    locales: list[str] = Field(default_factory=list)

    @field_validator("entrypoint")
    @classmethod
    def _validate_entrypoint(cls, v: str | None) -> str | None:
        if v is None:
            return v
        return _validate_entrypoint(v)

    @model_validator(mode="after")
    def _require_entrypoint_for_bundle_contributions(self) -> GcsBlock:
        if self.entrypoint is not None:
            return self
        for key in _GCS_BUNDLE_CONTRIBUTIONS:
            if getattr(self.contributes, key):
                raise ManifestError(
                    f"gcs.contributes.{key} is rendered from a bundle and needs "
                    "gcs.entrypoint"
                )
        return self

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
