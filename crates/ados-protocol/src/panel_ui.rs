//! Front-panel UI defaults shared by every consumer of `ground_station.ui`.
//!
//! The persisted panel config (`ground_station.ui.{oled,buttons,screens}`) is
//! read by three processes: the native control surface serves and writes it,
//! `ados-pic` resolves button presses through it, and `ados-display` builds its
//! page set from it. A key the operator never set falls back to the defaults
//! here, so the three agree on what an unconfigured panel does. The section
//! keeps its `ground_station` name on a visor (the config key is not renamed
//! per profile); only the defaults differ.
//!
//! Screen ids are the operator-facing names. A `plugin:<name>` entry is the
//! display's reserved plugin page (page id `plugin`) whose content a plugin
//! fills; `<name>` names which plugin page the operator expects there.

/// The profiles that carry a front panel (status display and buttons).
pub const PANEL_PROFILES: [&str; 2] = ["ground_station", "visor"];

/// True when `profile` (its wire spelling) carries a front panel.
pub fn has_panel(profile: &str) -> bool {
    PANEL_PROFILES.contains(&profile)
}

/// Button action reserved for plugins: the host's own consumers (display
/// navigator, cockpit relay) ignore a press carrying it, so only
/// `button.subscribe` subscribers act on it.
pub const PASSTHROUGH: &str = "passthrough";

/// Button action that opens the power-off confirmation on the panel.
pub const SHUTDOWN_PROMPT: &str = "shutdown_prompt";

/// Button action that steps the panel to the previous screen (the reverse of
/// `cycle_screen`).
pub const PREV_SCREEN: &str = "prev_screen";

/// Prefix of a screen id that names a plugin-filled page.
pub const PLUGIN_SCREEN_PREFIX: &str = "plugin:";

/// The world-engine capture HUD, the first visor screen.
pub const CAPTURE_HUD_SCREEN: &str = "plugin:capture_hud";

/// Ground-station default button mapping (short/long per front-panel button).
pub const GROUND_STATION_BUTTONS: [(&str, &str); 8] = [
    ("B1_short", "cycle_screen"),
    ("B1_long", "toggle_backlight"),
    ("B2_short", "show_network"),
    ("B2_long", "show_qr"),
    ("B3_short", "confirm"),
    ("B3_long", "pair_drone"),
    ("B4_short", "back"),
    ("B4_long", "menu"),
];

/// Visor default button mapping. B1/B2 short are REC and MARK for the capture
/// plugin; the long presses other than B1 are left to plugins as well.
pub const VISOR_BUTTONS: [(&str, &str); 8] = [
    ("B1_short", PASSTHROUGH),
    ("B1_long", SHUTDOWN_PROMPT),
    ("B2_short", PASSTHROUGH),
    ("B2_long", PASSTHROUGH),
    ("B3_short", "cycle_screen"),
    ("B3_long", PASSTHROUGH),
    ("B4_short", "back"),
    ("B4_long", PASSTHROUGH),
];

/// Default mapping of a board's rotary encoder, the same on every panel
/// profile and used only on a board that declares one. A detent is a `short`
/// press of `ENC_CW` (next screen) or `ENC_CCW` (previous screen); the push
/// switch is short (select) or long (back) like a button.
pub const ENCODER_BUTTONS: [(&str, &str); 4] = [
    ("ENC_CW_short", "cycle_screen"),
    ("ENC_CCW_short", PREV_SCREEN),
    ("ENC_SW_short", "confirm"),
    ("ENC_SW_long", "back"),
];

/// Ground-station default screen order (also the default enabled set).
pub const GROUND_STATION_SCREENS: [&str; 9] = [
    "home", "link", "drone", "network", "system", "qr", "gnss", "radios", "lora",
];

/// Visor default screen order (also the default enabled set).
pub const VISOR_SCREENS: [&str; 5] = [CAPTURE_HUD_SCREEN, "cameras", "sensors", "storage", "about"];

/// The default button mapping for `profile`. Profiles without a panel get the
/// ground-station table (their routes refuse the read anyway).
pub fn default_buttons(profile: &str) -> &'static [(&'static str, &'static str)] {
    if profile == "visor" {
        &VISOR_BUTTONS
    } else {
        &GROUND_STATION_BUTTONS
    }
}

/// The default screen order for `profile` (the same list is the default
/// enabled set).
pub fn default_screens(profile: &str) -> &'static [&'static str] {
    if profile == "visor" {
        &VISOR_SCREENS
    } else {
        &GROUND_STATION_SCREENS
    }
}

/// The plugin page name of a `plugin:<name>` screen id, `None` for a core
/// screen or an empty name.
pub fn plugin_screen_name(screen: &str) -> Option<&str> {
    screen
        .strip_prefix(PLUGIN_SCREEN_PREFIX)
        .filter(|name| !name.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visor_defaults_match_the_capture_layout() {
        let map: std::collections::HashMap<_, _> =
            default_buttons("visor").iter().copied().collect();
        assert_eq!(map["B1_short"], PASSTHROUGH);
        assert_eq!(map["B2_short"], PASSTHROUGH);
        assert_eq!(map["B3_short"], "cycle_screen");
        assert_eq!(map["B4_short"], "back");
        assert_eq!(map["B1_long"], SHUTDOWN_PROMPT);
        assert_eq!(
            default_screens("visor"),
            [
                "plugin:capture_hud",
                "cameras",
                "sensors",
                "storage",
                "about"
            ]
        );
    }

    #[test]
    fn ground_station_defaults_keep_their_map_and_add_the_new_screens() {
        let map: std::collections::HashMap<_, _> =
            default_buttons("ground_station").iter().copied().collect();
        assert_eq!(map["B1_short"], "cycle_screen");
        assert_eq!(map.len(), 8);
        assert!(default_screens("ground_station").ends_with(&["gnss", "radios", "lora"]));
        assert_eq!(default_buttons("drone"), default_buttons("ground_station"));
    }

    #[test]
    fn plugin_screen_ids_resolve_their_name() {
        assert_eq!(plugin_screen_name(CAPTURE_HUD_SCREEN), Some("capture_hud"));
        assert_eq!(plugin_screen_name("plugin:"), None);
        assert_eq!(plugin_screen_name("cameras"), None);
        assert!(has_panel("visor") && has_panel("ground_station") && !has_panel("drone"));
    }

    #[test]
    fn encoder_defaults_mirror_the_screen_carousel_and_never_shadow_a_button() {
        let map: std::collections::HashMap<_, _> = ENCODER_BUTTONS.iter().copied().collect();
        assert_eq!(map["ENC_CW_short"], "cycle_screen");
        assert_eq!(map["ENC_CCW_short"], PREV_SCREEN);
        assert_eq!(map["ENC_SW_short"], "confirm");
        assert_eq!(map["ENC_SW_long"], "back");
        for profile in PANEL_PROFILES {
            let buttons = default_buttons(profile);
            assert!(ENCODER_BUTTONS
                .iter()
                .all(|(key, _)| buttons.iter().all(|(b, _)| b != key)));
        }
    }
}
