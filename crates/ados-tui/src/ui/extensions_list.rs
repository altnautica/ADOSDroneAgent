//! The Extensions tab's left list: installed plugins first, then the catalog
//! entries not installed yet.

use serde_json::Value;

use crate::app::Core;
use crate::ext::state::{self, CatalogEntry, Install};
use crate::poll::ResourceId;

/// One left-list entry.
#[derive(Debug, Clone)]
pub(super) enum Entry {
    Installed(Install),
    Catalog(CatalogEntry),
}

impl Entry {
    /// What the selection remembers: the section and the plugin id.
    pub(super) fn key(&self) -> (bool, String) {
        match self {
            Entry::Installed(i) => (true, i.id.clone()),
            Entry::Catalog(c) => (false, c.id.clone()),
        }
    }
}

pub(super) fn entries(core: &Core) -> (Vec<Install>, Vec<CatalogEntry>) {
    let installs = core
        .value(&ResourceId::Plugins)
        .map(state::installs)
        .unwrap_or_default();
    let catalog = core
        .value(&ResourceId::PluginsCatalog)
        .map(state::catalog)
        .unwrap_or_default();
    let catalog = state::not_installed(catalog, &installs);
    (installs, catalog)
}

/// The display name of an installed plugin: the manifest name, else the
/// catalog name, else the id.
pub(super) fn name_of(core: &Core, id: &str) -> String {
    core.value(&ResourceId::PluginDetail(id.to_string()))
        .and_then(|d| crate::app::at(d, "manifest.name"))
        .and_then(Value::as_str)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .or_else(|| {
            core.value(&ResourceId::PluginsCatalog)
                .map(state::catalog)
                .and_then(|c| c.into_iter().find(|e| e.id == id).map(|e| e.name))
        })
        .unwrap_or_else(|| id.to_string())
}
