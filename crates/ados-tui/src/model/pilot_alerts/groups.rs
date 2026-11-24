//! The alert list's sections, from the cockpit spec's `alert-groups.json` as
//! the shared `status-pill.ts` reads it (`alertGroup`, `alertGroupTitle`,
//! `groupAlerts`): a readiness item by its readiness id, any other alert by
//! its id, else by its pill source; anything unmapped is `other`.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use serde::Deserialize;

use super::{PillAlert, Severity};

/// The spec file, byte for byte.
pub const ALERT_GROUPS_JSON: &str =
    include_str!("../../../../../web-shared/src/cockpit-spec/alert-groups.json");

/// One section of the alert list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertGroup {
    Setup,
    FlightController,
    Sensors,
    Navigation,
    Power,
    Link,
    System,
    Other,
}

impl AlertGroup {
    /// Every group, as the shared `ALERT_GROUPS` lists them.
    pub const ALL: [AlertGroup; 8] = [
        Self::Setup,
        Self::FlightController,
        Self::Sensors,
        Self::Navigation,
        Self::Power,
        Self::Link,
        Self::System,
        Self::Other,
    ];

    /// The spec's name (`flight_controller`).
    pub fn name(self) -> &'static str {
        match self {
            Self::Setup => "setup",
            Self::FlightController => "flight_controller",
            Self::Sensors => "sensors",
            Self::Navigation => "navigation",
            Self::Power => "power",
            Self::Link => "link",
            Self::System => "system",
            Self::Other => "other",
        }
    }

    /// A spec name as a group; an unknown one is `other`.
    fn from_name(name: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|g| g.name() == name)
            .unwrap_or(Self::Other)
    }
}

#[derive(Deserialize)]
struct Spec {
    readiness: BTreeMap<String, String>,
    id: BTreeMap<String, String>,
    source: BTreeMap<String, String>,
    order: Vec<String>,
    titles: BTreeMap<String, String>,
}

static SPEC: LazyLock<Spec> = LazyLock::new(|| {
    serde_json::from_str(ALERT_GROUPS_JSON).expect("alert-groups.json is the alert group spec")
});

/// The section of an alert with pill id `id` from pill source `source`.
pub fn alert_group(id: &str, source: &str) -> AlertGroup {
    let name = match id.strip_prefix("pilot:") {
        Some(item) => SPEC.readiness.get(item),
        None => SPEC.id.get(id).or_else(|| SPEC.source.get(source)),
    };
    name.map_or(AlertGroup::Other, |n| AlertGroup::from_name(n))
}

/// A section's title (`titles`); a group the spec does not title reads as
/// its name.
pub fn group_title(g: AlertGroup) -> String {
    SPEC.titles
        .get(g.name())
        .cloned()
        .unwrap_or_else(|| g.name().to_string())
}

/// One section of the alert list.
#[derive(Debug, Clone, PartialEq)]
pub struct AlertSection {
    pub group: AlertGroup,
    pub title: String,
    pub alerts: Vec<PillAlert>,
}

/// The alert list's sections, in the spec's group order; inside one, critical
/// before warning, then as raised. One row per text in a section (the worst
/// severity wins). Empty sections are left out. The terminal snoozes
/// nothing, so no row hides.
pub fn group_alerts(alerts: &[PillAlert]) -> Vec<AlertSection> {
    let mut by_group: Vec<(AlertGroup, Vec<PillAlert>)> = Vec::new();
    for a in alerts {
        let at = match by_group.iter().position(|(g, _)| *g == a.group) {
            Some(at) => at,
            None => {
                by_group.push((a.group, Vec::new()));
                by_group.len() - 1
            }
        };
        let rows = &mut by_group[at].1;
        match rows.iter_mut().find(|b| b.text == a.text) {
            Some(same) if a.severity == Severity::Critical => same.severity = Severity::Critical,
            Some(_) => {}
            None => rows.push(a.clone()),
        }
    }
    let mut out = Vec::new();
    for name in &SPEC.order {
        let group = AlertGroup::from_name(name);
        let Some(at) = by_group.iter().position(|(g, _)| *g == group) else {
            continue;
        };
        let (_, mut rows) = by_group.swap_remove(at);
        // Stable: equal severities keep the order they were raised in.
        rows.sort_by_key(|a| a.severity != Severity::Critical);
        out.push(AlertSection {
            group,
            title: group_title(group),
            alerts: rows,
        });
    }
    out
}
