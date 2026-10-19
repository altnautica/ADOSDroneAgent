//! The front-panel page set: which screens a node's panel shows, in order.
//!
//! The node profile and the persisted `ground_station.ui.screens` section
//! (`{order, enabled}`, the same key on a visor) decide it. A key the operator
//! never set falls back to [`ados_protocol::panel_ui::default_screens`] for the
//! profile, exactly as the agent's `/ui` read merges it, so the panel and the
//! config surfaces agree on what an unconfigured panel shows.
//!
//! The LCD navigator turns the resolved screens into its bottom-bar tab list
//! with [`tab_ids`]: a visor's screens map onto registered page ids, every
//! other profile keeps the ground-station tab set.

use std::path::{Path, PathBuf};

use ados_protocol::panel_ui;
use serde_norway::Value;

use crate::pages::plugin::PLUGIN_PAGE_ID;
use crate::pages::{GS_TAB_IDS, MAX_TABS};

/// The agent config read when `ADOS_CONFIG` is unset.
pub const DEFAULT_CONFIG_PATH: &str = "/etc/ados/config.yaml";

/// The profile whose panel tabs follow its screen list.
const VISOR_PROFILE: &str = "visor";

/// The page id the `about` screen resolves to. The about page keeps its
/// full-screen chrome as a tab: the cycle and back buttons move off it.
pub const ABOUT_PAGE_ID: &str = "details.about";

/// The resolved panel config of this node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelConf {
    /// The node profile in the underscore spelling [`panel_ui`] keys on
    /// (`ground_station`, `visor`).
    pub profile: String,
    /// The screen ids the panel shows, in order (never empty).
    pub screens: Vec<String>,
}

impl PanelConf {
    /// Resolve from the agent config at `ADOS_CONFIG` (default
    /// [`DEFAULT_CONFIG_PATH`]) and the profile sentinel. Any missing or
    /// malformed source falls back to the profile defaults.
    pub fn load() -> Self {
        let config = std::env::var_os("ADOS_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH));
        Self::load_from(&config, &ados_config::profile_conf_path())
    }

    /// [`PanelConf::load`] against explicit paths.
    pub fn load_from(config_yaml: &Path, profile_conf: &Path) -> Self {
        // The resolver returns the hyphen wire form; the panel tables key on
        // the underscore spelling.
        let profile = ados_config::node_profile_at(config_yaml, profile_conf).replace('-', "_");
        let root = std::fs::read_to_string(config_yaml)
            .ok()
            .and_then(|text| serde_norway::from_str::<Value>(&text).ok());
        let screens_node = root
            .as_ref()
            .and_then(|r| r.get("ground_station")?.get("ui")?.get("screens"));
        let screens = resolve_screens(&profile, screens_node);
        Self { profile, screens }
    }

    /// The navigator tab list for this config.
    pub fn tab_ids(&self) -> Vec<&'static str> {
        tab_ids(&self.profile, &self.screens)
    }
}

/// The screens `profile` shows from a `ground_station.ui.screens` node: the
/// `order` entries that are also `enabled`, first occurrence kept. An absent or
/// non-list `order` / `enabled` is the profile default list, and a resolution
/// that leaves nothing falls back to the defaults, since a panel with no
/// screens has nothing to show.
pub fn resolve_screens(profile: &str, screens: Option<&Value>) -> Vec<String> {
    let defaults = || -> Vec<String> {
        panel_ui::default_screens(profile)
            .iter()
            .map(|s| (*s).to_string())
            .collect()
    };
    let list = |key: &str| -> Vec<String> {
        screens
            .and_then(|s| s.get(key))
            .and_then(Value::as_sequence)
            .map(|seq| {
                seq.iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_else(defaults)
    };
    let enabled = list("enabled");
    let mut out: Vec<String> = Vec::new();
    for id in list("order") {
        if enabled.contains(&id) && !out.contains(&id) {
            out.push(id);
        }
    }
    if out.is_empty() {
        return defaults();
    }
    out
}

/// The LCD page id a screen id shows as: any `plugin:<name>` is the reserved
/// plugin page, `about` the about page, and the visor status screens their own
/// pages. `None` for a screen the LCD has no page for.
pub fn screen_page_id(screen: &str) -> Option<&'static str> {
    if panel_ui::plugin_screen_name(screen).is_some() {
        return Some(PLUGIN_PAGE_ID);
    }
    match screen {
        "cameras" => Some("cameras"),
        "sensors" => Some("sensors"),
        "storage" => Some("storage"),
        "about" => Some(ABOUT_PAGE_ID),
        _ => None,
    }
}

/// The navigator tab list for `profile` showing `screens`. A visor's tabs are
/// its screens mapped through [`screen_page_id`] (unknown screens skipped,
/// repeats collapsed, at most [`MAX_TABS`]); when none map, its default screens
/// are used. Every other profile keeps [`GS_TAB_IDS`].
pub fn tab_ids(profile: &str, screens: &[String]) -> Vec<&'static str> {
    if profile != VISOR_PROFILE {
        return GS_TAB_IDS.to_vec();
    }
    let map = |screens: &mut dyn Iterator<Item = &str>| -> Vec<&'static str> {
        let mut tabs: Vec<&'static str> = Vec::with_capacity(MAX_TABS);
        for id in screens.filter_map(screen_page_id) {
            if tabs.len() == MAX_TABS {
                break;
            }
            if !tabs.contains(&id) {
                tabs.push(id);
            }
        }
        tabs
    };
    let tabs = map(&mut screens.iter().map(String::as_str));
    if tabs.is_empty() {
        return map(&mut panel_ui::default_screens(profile).iter().copied());
    }
    tabs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn yaml(text: &str) -> Value {
        serde_norway::from_str(text).unwrap()
    }

    #[test]
    fn absent_section_is_the_profile_default() {
        assert_eq!(
            resolve_screens("visor", None),
            panel_ui::VISOR_SCREENS.map(String::from).to_vec()
        );
        assert_eq!(
            resolve_screens("ground_station", None),
            panel_ui::GROUND_STATION_SCREENS.map(String::from).to_vec()
        );
    }

    #[test]
    fn order_is_filtered_by_enabled() {
        let node = yaml(
            "order: [storage, \"plugin:capture_hud\", about, cameras]\n\
             enabled: [cameras, \"plugin:capture_hud\", storage]\n",
        );
        assert_eq!(
            resolve_screens("visor", Some(&node)),
            ["storage", "plugin:capture_hud", "cameras"]
        );
    }

    #[test]
    fn a_missing_key_takes_the_default_list() {
        // Only `enabled` set: the default order, filtered.
        let node = yaml("enabled: [sensors, about]\n");
        assert_eq!(resolve_screens("visor", Some(&node)), ["sensors", "about"]);
        // Only `order` set: everything in it the defaults enable.
        let node = yaml("order: [about, ghost, storage]\n");
        assert_eq!(resolve_screens("visor", Some(&node)), ["about", "storage"]);
    }

    #[test]
    fn nothing_enabled_falls_back_to_defaults() {
        let node = yaml("order: [cameras]\nenabled: []\n");
        assert_eq!(
            resolve_screens("visor", Some(&node)),
            panel_ui::VISOR_SCREENS.map(String::from).to_vec()
        );
    }

    #[test]
    fn visor_default_tabs() {
        let screens = resolve_screens("visor", None);
        assert_eq!(
            tab_ids("visor", &screens),
            ["plugin", "cameras", "sensors", "storage", "details.about"]
        );
    }

    #[test]
    fn plugin_and_unknown_screens_map() {
        assert_eq!(screen_page_id("plugin:capture_hud"), Some(PLUGIN_PAGE_ID));
        assert_eq!(screen_page_id("plugin:"), None);
        assert_eq!(screen_page_id("home"), None);
        let screens: Vec<String> = ["home", "plugin:a", "storage", "plugin:b", "ghost"]
            .map(String::from)
            .to_vec();
        assert_eq!(tab_ids("visor", &screens), ["plugin", "storage"]);
        // Nothing the LCD can show: the visor defaults.
        let screens = vec!["home".to_string()];
        assert_eq!(tab_ids("visor", &screens).len(), MAX_TABS);
    }

    #[test]
    fn other_profiles_keep_the_ground_station_tabs() {
        let screens = resolve_screens("visor", None);
        for profile in ["ground_station", "drone", "workstation", "compute"] {
            assert_eq!(tab_ids(profile, &screens), GS_TAB_IDS);
        }
    }

    #[test]
    fn load_reads_profile_and_screens_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        let pc = dir.path().join("profile.conf");
        std::fs::write(
            &cfg,
            "agent:\n  profile: visor\nground_station:\n  ui:\n    screens:\n      \
             order: [about, \"plugin:capture_hud\"]\n",
        )
        .unwrap();
        let conf = PanelConf::load_from(&cfg, &pc);
        assert_eq!(conf.profile, "visor");
        assert_eq!(conf.screens, ["about", "plugin:capture_hud"]);
        assert_eq!(conf.tab_ids(), ["details.about", "plugin"]);

        // No config at all: the profile sentinel decides, defaults fill in.
        std::fs::write(&pc, "profile: ground_station\n").unwrap();
        let conf = PanelConf::load_from(&dir.path().join("absent.yaml"), &pc);
        assert_eq!(conf.profile, "ground_station");
        assert_eq!(conf.tab_ids(), GS_TAB_IDS);
    }
}
