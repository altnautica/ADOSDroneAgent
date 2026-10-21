//! Tab identities: digit order, labels, and per-profile visibility.

/// The tabs, in digit order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TabId {
    Overview,
    Vehicle,
    Link,
    Video,
    Params,
    Extensions,
    Logs,
    System,
    Settings,
}

impl TabId {
    pub const ALL: [TabId; 9] = [
        TabId::Overview,
        TabId::Vehicle,
        TabId::Link,
        TabId::Video,
        TabId::Params,
        TabId::Extensions,
        TabId::Logs,
        TabId::System,
        TabId::Settings,
    ];

    pub fn index(self) -> usize {
        Self::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }

    pub fn digit(self) -> char {
        char::from(b'1' + self.index() as u8)
    }

    /// The name stored as `last_tab` in the preferences.
    pub fn name(self) -> &'static str {
        match self {
            TabId::Overview => "overview",
            TabId::Vehicle => "vehicle",
            TabId::Link => "link",
            TabId::Video => "video",
            TabId::Params => "params",
            TabId::Extensions => "extensions",
            TabId::Logs => "logs",
            TabId::System => "system",
            TabId::Settings => "settings",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.name() == name)
    }

    /// The tab-bar label; slot 2 reads "Ground" on a ground station.
    pub fn label(self, profile: &str) -> &'static str {
        match self {
            TabId::Overview => "Overview",
            TabId::Vehicle if profile == "ground_station" => "Ground",
            TabId::Vehicle => "Vehicle",
            TabId::Link => "Link",
            TabId::Video => "Video",
            TabId::Params => "Params",
            TabId::Extensions => "Extensions",
            TabId::Logs => "Logs",
            TabId::System => "System",
            TabId::Settings => "Settings",
        }
    }

    /// The short label used below 100 columns.
    pub fn short(self, profile: &str) -> &'static str {
        match self {
            TabId::Overview => "Ovw",
            TabId::Vehicle if profile == "ground_station" => "Gnd",
            TabId::Vehicle => "Veh",
            TabId::Link => "Lnk",
            TabId::Video => "Vid",
            TabId::Params => "Prm",
            TabId::Extensions => "Ext",
            TabId::Logs => "Log",
            TabId::System => "Sys",
            TabId::Settings => "Set",
        }
    }

    /// Whether the tab shows on a node with this profile. An unreported or
    /// undecided profile shows every tab, matching the web dashboard.
    pub fn visible(self, profile: &str) -> bool {
        if !matches!(profile, "drone" | "ground_station" | "visor") {
            return true;
        }
        match self {
            TabId::Vehicle => true,
            TabId::Link => matches!(profile, "drone" | "ground_station"),
            TabId::Video | TabId::Params => matches!(profile, "drone" | "visor"),
            _ => true,
        }
    }
}

/// The display name of a profile, for refusal toasts.
pub fn profile_label(profile: &str) -> &str {
    match profile {
        "ground_station" => "ground station",
        other => other,
    }
}
