//! The GCS half, compatibility constraints, and board identity.

use super::*;

/// GCS-half manifest block. Only `entrypoint` + `isolation` + `permissions`
/// are read by the controller; the page and surface contributions are
/// validated here and projected by [`gcs_block_json`].
///
/// `entrypoint` names the bundle the GCS loads into an iframe or inline. It
/// may be omitted only when the half contributes nothing rendered from a
/// bundle (no `panels`, `tabs`, `agent_pages` or `node_surfaces`), so a
/// parameters- or skills-only half needs no bundle at all.
#[derive(Debug, Clone, Deserialize)]
pub struct GcsBlock {
    #[serde(default)]
    pub entrypoint: Option<String>,
    #[serde(default)]
    pub isolation: GcsIsolation,
    #[serde(default)]
    pub permissions: Vec<PermissionRef>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_norway::Value>,
}

impl GcsBlock {
    pub(super) fn contribution<T: serde::de::DeserializeOwned>(
        &self,
        key: &str,
    ) -> Result<Vec<T>, ManifestError> {
        match self.extra.get("contributes").and_then(|c| c.get(key)) {
            None => Ok(Vec::new()),
            Some(raw) => serde_norway::from_value(raw.clone())
                .map_err(|e| ManifestError(format!("gcs.contributes.{key} is malformed: {e}"))),
        }
    }

    /// The Agent-sidebar pages this half contributes.
    pub fn agent_pages(&self) -> Vec<AgentPage> {
        self.contribution("agent_pages").unwrap_or_default()
    }

    /// The node surfaces this half contributes.
    pub fn node_surfaces(&self) -> Vec<NodeSurface> {
        self.contribution("node_surfaces").unwrap_or_default()
    }

    pub(super) fn validate_contributions(&self) -> Result<(), ManifestError> {
        let pages: Vec<AgentPage> = self.contribution("agent_pages")?;
        let surfaces: Vec<NodeSurface> = self.contribution("node_surfaces")?;
        let mut page_ids: BTreeSet<&str> = BTreeSet::new();
        for page in &pages {
            validate_contribution_id("gcs.contributes.agent_pages", &page.id)?;
            validate_title("gcs.contributes.agent_pages", &page.id, &page.title)?;
            if let Some(profiles) = &page.profile {
                validate_profile_list(
                    &format!("gcs.contributes.agent_pages[{}].profile", page.id),
                    profiles,
                )?;
            }
            if !page_ids.insert(page.id.as_str()) {
                return Err(ManifestError(format!(
                    "gcs.contributes.agent_pages id {:?} appears twice",
                    page.id
                )));
            }
        }
        for page in &pages {
            let Some(target) = page.setup_for.as_deref() else {
                continue;
            };
            let host = pages.iter().find(|p| p.id == target);
            if target == page.id || host.is_none() || host.is_some_and(|h| h.setup_for.is_some()) {
                return Err(ManifestError(format!(
                    "gcs.contributes.agent_pages[{}].setup_for {target:?} must name another \
                     page of this plugin that is not itself a setup page",
                    page.id
                )));
            }
        }
        let mut surface_ids: BTreeSet<&str> = BTreeSet::new();
        for surface in &surfaces {
            validate_contribution_id("gcs.contributes.node_surfaces", &surface.id)?;
            validate_title("gcs.contributes.node_surfaces", &surface.id, &surface.title)?;
            validate_profile_list(
                &format!("gcs.contributes.node_surfaces[{}].profile", surface.id),
                &surface.profile,
            )?;
            if let Some(group) = &surface.group {
                if !NODE_SURFACE_GROUPS.contains(&group.as_str()) {
                    return Err(ManifestError(format!(
                        "gcs.contributes.node_surfaces[{}].group {group:?} must be one of \
                         {NODE_SURFACE_GROUPS:?}",
                        surface.id
                    )));
                }
            }
            if !surface_ids.insert(surface.id.as_str()) {
                return Err(ManifestError(format!(
                    "gcs.contributes.node_surfaces id {:?} appears twice",
                    surface.id
                )));
            }
        }
        Ok(())
    }

    /// The first contribution list that renders from the bundle and is
    /// non-empty, if any.
    pub(super) fn bundle_contribution(&self) -> Option<&'static str> {
        let contributes = self.extra.get("contributes")?;
        GCS_BUNDLE_CONTRIBUTIONS.iter().copied().find(|key| {
            contributes
                .get(*key)
                .and_then(|v| v.as_sequence())
                .is_some_and(|s| !s.is_empty())
        })
    }
}

/// The GCS contribution lists rendered from the half's bundle (an iframe or
/// an inline module); declaring any of them requires `gcs.entrypoint`.
pub(super) const GCS_BUNDLE_CONTRIBUTIONS: &[&str] =
    &["panels", "tabs", "agent_pages", "node_surfaces"];

/// The free-form GCS contribution lists the detail projection carries through
/// unchanged, in the order they are emitted.
pub(super) const GCS_FREEFORM_CONTRIBUTIONS: &[&str] = &[
    "panels",
    "overlays",
    "notifications",
    "skills",
    "tabs",
    "parameters",
    "models",
    "target_actions",
    "settings",
    "map_overlays",
    "mission_templates",
];

/// The `manifest.gcs` block of the plugin detail route: the GCS half's
/// entrypoint, isolation, slot contributions and locales, so a LAN-paired GCS
/// can build the contribution set and locate the bundle with no cloud. `None`
/// for a plugin with no GCS half.
pub fn gcs_block_json(manifest: &PluginManifest) -> Option<serde_json::Value> {
    let gcs = manifest.gcs.as_ref()?;
    let contributes = gcs.extra.get("contributes");
    let mut map = serde_json::Map::new();
    for key in GCS_FREEFORM_CONTRIBUTIONS {
        let list = contributes
            .and_then(|c| c.get(*key))
            .filter(|v| v.is_sequence())
            .and_then(|v| serde_json::to_value(v).ok())
            .unwrap_or_else(|| serde_json::Value::Array(Vec::new()));
        map.insert((*key).to_string(), list);
    }
    map.insert(
        "agent_pages".to_string(),
        serde_json::to_value(gcs.agent_pages()).unwrap_or_default(),
    );
    map.insert(
        "node_surfaces".to_string(),
        serde_json::to_value(gcs.node_surfaces()).unwrap_or_default(),
    );
    let locales = gcs
        .extra
        .get("locales")
        .filter(|v| v.is_sequence())
        .and_then(|v| serde_json::to_value(v).ok())
        .unwrap_or_else(|| serde_json::Value::Array(Vec::new()));
    Some(serde_json::json!({
        "entrypoint": gcs.entrypoint,
        "isolation": gcs.isolation.as_str(),
        "contributes": serde_json::Value::Object(map),
        "locales": locales,
    }))
}

/// Compatibility constraints checked at install time.
#[derive(Debug, Clone, Deserialize)]
pub struct Compatibility {
    /// Semver range, e.g. `>=0.9.0,<1.0.0`.
    pub ados_version: String,
    #[serde(default)]
    pub gcs_version: Option<String>,
    /// Board tokens this plugin supports: a HAL profile stem (`cm4`,
    /// `rock-5c-lite`) or a SoC (`rk3588s2`). Empty means any board, and so
    /// does a list containing the literal `"*"`.
    #[serde(default)]
    pub supported_boards: Vec<String>,
    /// Minimum compute tier (1-4) the plugin needs. `None` means no floor.
    ///
    /// Present here because the cloud-relay install path runs this parser, and
    /// dropping the field made the tier floor enforceable over LAN but not
    /// over the relay: an NPU-dependent plugin refused on a tier-1 board by
    /// the Python supervisor installed cleanly when pushed from the cloud, then
    /// crash-looped at runtime instead of being refused up front.
    #[serde(default)]
    pub min_tier: Option<u8>,
}

/// The running board's identity the `supported_boards` gate keys on: the HAL
/// profile stem (the canonical token, the one `/etc/ados/board_override`
/// accepts) and the SoC (so archives that list a SoC keep matching).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardId {
    pub profile: String,
    /// Empty when the sidecar reports the SoC as unknown.
    pub soc: String,
}

impl std::fmt::Display for BoardId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.profile)
    }
}

impl Compatibility {
    /// True when `board` satisfies `supported_boards`: the list is empty, an
    /// entry is `"*"`, or an entry equals the profile stem or the (non-empty)
    /// SoC, case-insensitively.
    pub fn supports_board(&self, board: &BoardId) -> bool {
        self.supported_boards.is_empty()
            || self.supported_boards.iter().any(|b| {
                b == "*"
                    || b.eq_ignore_ascii_case(&board.profile)
                    || (!board.soc.is_empty() && b.eq_ignore_ascii_case(&board.soc))
            })
    }
}

pub(super) fn default_risk() -> String {
    "medium".to_string()
}
